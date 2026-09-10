# Python bindings — plan

Status: **planned**, not started. Decided 2026-09-10; work begins after the
next kaish release lands, and this document is what that work starts from.
Everything below was checked against kaish 0.17.2 at the source, and each
claim about kaish names the place it was read.

## What it is

A `kaish` package on PyPI: the kaish kernel behind a thin pyo3 layer, built
with maturin from `python/` in this repo. It replaces `subprocess.run(['kaish',
'--plan-file', '-'])` in Python programs with a function call, and gives a
Python program a kernel it can execute scripts in.

The first consumers are the analysis scripts in lfm2d, which plan Bash
commands and never execute them. The intended later consumer is a Python
agent: a fastmcp server that offers a `run_kaish` tool to any MCP client. The
second consumer is why some decisions below are made now rather than when it
arrives.

## Why here, not in kaish

This repo is an honest external embedder: it consumes kaish through the
published crates and nothing else, and when it needs a boundary that does not
exist, the fix is a kaish PR. Bindings are the purest embedder kaish has, and
the one most likely to find missing boundaries, because every type a Python
program touches has to cross the language line. Finding those from outside is
the point.

Two more reasons. pyo3 and maturin stay out of kaish core, which stays lean.
And the pinning discipline lives here already: a wheel pins a published kaish
version and moves to the next minor as a deliberate change that has to build
and pass (see `AGENTS.md`, "kaish dependency pinning").

`kaish-wasi` living in kaish is not a precedent. It is `publish = false` with
path-only dependencies. A published wheel is the opposite of that.

## The callers this is shaped by

All in `~/src/lfm2d`, all shelling out to `kaish --plan-file -`:

- `lfm2d/hooks/kaish_plan.py` runs one plan per Bash call under a 0.5 s
  timeout with a never-raises fallback, and stamps every row with the kaish
  version string, because the canonical rendering is the scored text and it
  changes across releases.
- `training/v10/kaish_floor.py`, `bash_to_kaish.py`, and
  `glob_class_floor.py` do the same in bulk over thousands of commands.

None of them execute anything. `plan()` ships first and alone; `execute()`
comes second.

## Findings from the kaish source

- **Planning needs no kernel.** `kaish_kernel::plan_program` is a pure
  function of the source text (`crates/kaish-kernel/src/kernel.rs`, the
  `plan_program` method delegates to `ast::plan::plan_program`). No tokio, no
  filesystem, no capability. `kaish.plan()` costs microseconds.
- **The plan document envelope is not a library type.** The object
  `--plan-file` prints, with `statements`, `kaish_version`, `kaish_git_hash`,
  and `kaish_build_date`, is assembled by hand in
  `crates/kaish-repl/src/main.rs` (three sites) and again in the `plan`
  builtin (`crates/kaish-kernel/src/tools/builtin/plan.rs`). Bindings that
  reproduce it hold a copy that can drift. See "kaish PRs this needs".
- **Execution is async and self-locking.** `Kernel::execute` and its
  `_with_options` variants are `async fn`; the kernel takes its own execute
  lock, so calls from several Python threads serialize inside kaish rather
  than needing a lock in the bindings.
- **Failure is mostly in-band.** A non-zero exit is an ordinary `ExecResult`.
  A request timeout lands as exit 124 (`kernel.rs`, `run_with_timeout`), and
  a cancellation as 130. `KernelError` has three shapes: `Parse` with the
  errors list and spans, `Validation` with the issues list, and a runtime
  error carrying the original chain (`crates/kaish-kernel/src/error.rs`).
- **`ExecResult` is `#[non_exhaustive]`** with `out` as an `OutputPayload`
  behind accessors, text or bytes (`crates/kaish-types/src/result.rs`).
  `Value` has seven variants: Null, Bool, Int, Float, String, Json, Bytes
  (`crates/kaish-types/src/value.rs`).
- **kaibo is the reference for the agent-facing kernel.** Its `run_kaish`
  builds `KernelConfig::agent().with_cwd(root)
  .with_allow_external_commands(false).with_request_timeout(..)
  .with_output_limit(..)` (`~/src/kaibo/src/sandbox.rs`), composes the tool
  description from the live kernel's help
  (`kaish_syntax.rs::run_kaish_tool_description`), and mounts a read-only
  backend through `Kernel::with_backend`. The first two are within reach of a
  thin binding. The backend is not, and is deferred with the rest of the
  "bigger idea" (see "Out of scope").

## The API

Module `kaish`. Types are typed Python classes, each with a `to_dict()` that
matches the JSON kaish itself prints, byte for byte where kaish prints one.
Amy's call, 2026-09-10: `plan()` stays typed with `.to_dict()`, rather than
returning the raw dict.

```python
import kaish

doc = kaish.plan("echo hi > out.txt | grep x")
doc.statements[0].plan.commands[0].name   # "echo"
doc.kaish_version                          # "0.17.2"
doc.to_dict()                              # the --plan-file object

k = kaish.Kernel(kaish.Config.agent(cwd="/repo", allow_external_commands=False,
                                    request_timeout=30.0))
r = k.execute("ls | head -3")
r.code, r.out, r.err, r.data, r.did_spill
r.to_dict()
```

Decisions, each made for the fastmcp consumer as much as for lfm2d:

1. **Synchronous, GIL released during `execute`.** fastmcp runs a sync tool
   on a thread pool, so a sync binding is enough and no pyo3-asyncio bridge is
   built until a caller needs `await`. The GIL is released across the call so
   a server stays responsive while a script runs.
2. **One `Kernel` per session, thread-safe.** A current-thread tokio runtime
   owned by the Python object, `block_on` per call. This is the kaish-web
   shape (`crates/kaish-web`), and native tokio has what wasm lacks: timers
   work, so `request_timeout` and `sleep` are real.
3. **Exit codes stay in-band.** `execute()` returns for any exit code,
   including 124 and 130. It raises only for what kaish itself reports as an
   error before or outside execution:
   - `kaish.KaishError` — base class.
   - `kaish.ParseError` — carries `errors`, a list of `(message, start, end)`,
     and `message`, the pre-rendered text.
   - `kaish.ValidationError` — carries `issues` with each issue's code,
     message, and span.
   - `kaish.KernelError` — a runtime fault, with the chain rendered into
     `str(e)`.
   `plan()` raises `ParseError` with the same shape. lfm2d's never-raises
   wrapper stays in lfm2d, where the fallback policy belongs.
4. **`ExecResult` exposes `code`, `out` (`str`, or `bytes` when the payload is
   binary), `err`, `data`, `did_spill`, and `to_dict()`.** A tool result has
   to serialize. `data` maps `Value` directly: Null to `None`, Bool, Int,
   Float, String to their Python types, Json to the parsed object, Bytes to
   `bytes`. A `Value` variant the bindings do not know raises rather than
   becoming `None`.
5. **`tool_schemas()` and `help(topic)` exist from the first `execute`
   release.** An agent-facing `run_kaish` needs a description composed from
   the live kernel, and kaibo already learned that the hard way. Both return
   typed objects with `to_dict()`.
6. **Two version constants.** `kaish.KAISH_VERSION` and
   `kaish.KAISH_GIT_HASH` are the kernel's (`kaish_kernel::KAISH_VERSION`),
   and `kaish.__version__` is the wheel's. lfm2d windows rows against the
   renderer, not the package, and the two move on different cadences.
7. **`Config` mirrors `KernelConfig`.** Classmethods `isolated()`,
   `transient()`, `agent(root=None)`, `repl()`, each taking the `with_*`
   builders as keyword arguments: `cwd`, `allow_external_commands`,
   `request_timeout`, `output_limit`, `vars`, `errexit`, `trash`,
   `vfs_budget`. A keyword the kernel does not have is a `TypeError`, not an
   ignored key.
8. **Typed stubs ship with the wheel.** A `kaish/__init__.pyi` so editors and
   type checkers see the API; the stub is checked in and a test asserts it
   matches the runtime module.

Every docstring and every exception message is published text and carries
full writing-style weight (`AGENTS.md`, "Writing style").

## Layout

```
python/
  Cargo.toml        # its own [workspace]; pins kaish "0.17" like the root
  Cargo.lock
  pyproject.toml    # maturin, package name kaish
  src/lib.rs        # the pyo3 module
  kaish/__init__.py
  kaish/__init__.pyi
  tests/            # pytest
```

`python/` is its own cargo workspace and the root `Cargo.toml` gains
`exclude = ["python"]`. The reason is the feature rule in `AGENTS.md`: the
native package turns the kernel's default features on (`localfs`, `overlay`),
and a member of the root workspace doing that would let them into the
wasm build's feature unification. Two workspaces means two lockfiles, so CI
asserts that the kaish pin string in `python/Cargo.toml` equals the root's.

## Checks

- **pytest is the check**, in the same spirit as smoke.html and e2e: contract
  tests against the real kernel, in the shape of lfm2d's
  `lfm2d/hooks/test_kaish_plan.py`. A behavior change on a later kaish is
  signal, not test rot.
- A test that `plan(src).to_dict()` equals `json.loads` of `kaish --plan-file
  -` for a corpus of commands, while the binary is on PATH. This is the
  drift detector for the envelope until kaish owns it.
- A test that every `Value` variant round-trips, with a negative control that
  an unknown variant raises.
- A test that `execute` releases the GIL: a second thread makes progress
  during a `sleep 1`.
- CI: `maturin build`, pytest on the built wheel, clippy on `python/`, and the
  pin-equality assertion. Wheels for Linux first; macOS and Windows when
  someone needs them.

## kaish PRs this needs

- **A `PlanDocument` type.** Move the envelope out of the repl binary and the
  `plan` builtin into kaish-kernel, so binary, builtin, and bindings serialize
  one shape. Open this first; until it lands the bindings carry a copy and the
  drift test above is what guards it.
- Anything the `Value` and `ExecResult` mapping turns up. Neither is expected
  to, but the first mapping is where a missing accessor shows.

Workflow as in `AGENTS.md`: open the kaish PR, pin `python/` to the branch to
develop against it, move back to the published version when the release
carrying it lands.

## First steps, in order

1. Skeleton: `python/` with `plan()` only, `PlanDocument` typed with
   `to_dict()`, the version constants, and the drift test.
2. `maturin develop` into lfm2d's `.venv-train`; swap `kaish_plan.py` to
   `kaish.plan()` behind its existing never-raises wrapper; measure the hook's
   per-call time against the 12 ms subprocess baseline.
3. `Kernel`, `Config`, `execute`, `ExecResult`, the exception hierarchy, the
   GIL test.
4. `tool_schemas()` and `help()`.
5. The fastmcp demo under `python/examples/`: one `run_kaish` tool over an
   `agent` kernel with external commands off and an output limit, description
   composed from `help`.

## Out of scope

- **Python-implemented tools.** `Tool::execute` is `async fn` taking an
  `ExecContext` that is `#[non_exhaustive]`; a Python tool needs a bridge onto
  the tokio runtime and a stable context shape. This is the bigger idea and
  it waits for a real use.
- **A Python backend** (`Kernel::with_backend`), the read-only VFS kaibo uses.
  Same tier.
- **Streaming output** (`execute_streaming`) and **pipe stdin**. Add when a
  caller needs them; the sync shape accommodates a callback.
- **An async API.** See decision 1.
