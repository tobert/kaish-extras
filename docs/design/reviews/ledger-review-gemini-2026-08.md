# Approval-ledger review — gemini-pro (batch, max thinking), 2026-08-01

Cross-model review of [../approval-ledger.md](../approval-ledger.md) via kaibo
deliberate (`gemini-deliberate` cast; dossier built by gemini-flash-lite over
the kaish tree + attached design docs). Verbatim below; synthesis and adopted
amendments live in the design doc's "Cross-model review synthesis" section.

---

**VERDICT & EXECUTIVE SUMMARY**

The Approval Ledger design is architecturally sound and clears the path for the git plugin's write operations (`git.md` §7, `architecture.md` §G.3). By adopting a replay-not-resume model, strictly separating `Requester` from `Approvals` at the type level, and formalizing the state machine, it successfully graduates the kernel from an ephemeral speed bump (`NonceStore`) to a rigorous authorization capability.

**Recommendation:** Proceed to implementation (Ship it), contingent on mitigating four specific blockers identified below.

### PRIORITIZED FINDINGS & BLOCKERS

1. **[Severity: High / Blocker] Data Type Migration Hazard in `--confirm`:** Replacing the non-CSPRNG `u32` generated in `crates/kaish-kernel/src/nonce.rs:46-191` with a secure token (likely UUID/String) will silently break CLI argument parsers or wrappers expecting an integer for `--confirm=<token>`. This threatens the stability contract.
2. **[Severity: High / Blocker] Deadlock in `Approver::decide`:** The 4-stage decision chain (§C.2) introduces a catastrophic deadlock risk if the internal Ledger lock is held while `Approver::decide` awaits human or LLM input. Lock release prior to yield is mandatory.
3. **[Severity: Medium] Ring Buffer Eviction Violates Balance Rule:** Bounding the ring buffer to 1024 entries means the checkability of the balance rule ("every `Redeemed` has a live `Granted` ancestor") will degrade under high-throughput auto-approvals, breaking offline audits if the `LedgerSink` backs up or drops data.
4. **[Severity: Medium] Zombie Ledger Entries on Cancellation:** The dispatch-seam auto-settle (`crates/kaish-kernel/src/kernel.rs:3321-3322`) must rely on a strict Rust `Drop` guard, not just `match` statements, to ensure canceled futures or background job evictions correctly settle entries as `Outcome::Cancelled`.

---

### 1. CORRECTNESS (State Machine & Concurrency)

**State Machine & Concurrency (§B):** The transition matrix is unambiguous and the `Requested` -> `Granted` -> `Redeeming` -> `Settled` flow is solid. The adoption of replay-not-resume with redemption-time precondition verification (`StateResolver::observe`) is the correct architectural choice to prevent TOCTOU (Time-Of-Check to Time-Of-Use) vulnerabilities under concurrent kernels. `Kernel::confirm` (`crates/kaish-kernel/src/kernel.rs:1594-1619`) handles the replay, and if the filesystem/environment state drifts between Grant and Redeem, the `StateClaim` will mismatch, forcing a `Refused` state.

**Background Jobs (§B.4):** Preserving `JobStatus::Latched` (`crates/kaish-kernel/src/scheduler/job.rs:188-232`) but backing it with a persistent ledger request resolves the historical "dead-nonce-forever" bug. Expiries correctly materialize as `Expired`.

**The Evidence Gap & Assumption:** The dossier claims the balance rule is "mathematically checkable." *Assumption:* I am assuming the in-memory ledger and the `LedgerSink` share a strict transaction sequence ID. *If this is wrong*, eviction from the 1024-limit ring buffer will create orphaned `Redeemed` records in memory that point to `Granted` records that only exist in the sink, breaking runtime invariant checks. *Correction required:* In-memory invariant checks must gracefully accept "Ancestor in Sink" or keep active chains in memory indefinitely.

### 2. SECURITY MODEL HONESTY

**Type-Level Separation:** The separation of `Requester` and `Approvals` types closes the loop on in-process capability leaking.

**Agent Self-Approval Hole (§D.3):** Handing a bearer token directly to an agent via `ExecResult.latch` (`crates/kaish-types/src/result.rs:208`) was the core vulnerability of the old system. The proposal correctly identifies that redacting this token in agent sessions and guarding `approvals grant` via configuration shuts this down.

**The Evidence Gap & Assumption:** *Assumption:* The agent cannot read kernel-level stderr/stdout prompts where the token might be printed for a human, nor can it bypass the VFS projection (`/v/approvals`). *If the agent's PTY or logging configuration blends kernel prompt streams with tool output streams*, the agent can still scrape the token and self-approve. *Correction required:* You must ensure strict stream segregation for the approval prompt (e.g., via a dedicated out-of-band control channel or `/dev/tty` masking).

### 3. API DESIGN

**`ToolCtx::request_approval`:** The fail-closed default (Exit code 2) is exactly right for zero-trust environments.

**Deadlocks & The Ledger Lock:** The 4-stage decision chain (Standing -> Policy -> Decide -> Defer) introduces asynchronous waiting. In Rust, holding a synchronous `MutexGuard` across an `.await` is a compile error, but holding an async Mutex or an `RwLock` read-guard while waiting on a slow human/LLM approver will stall all concurrent ledger operations. `Approver::decide` must operate strictly on cloned/owned request snapshots, and re-acquire the lock only to apply the decision.

**Dispatch-Seam Auto-Settle (§C.1):** Relying on the dispatch seam (`crates/kaish-kernel/src/tools/context.rs:759-800`) to settle entries requires a `Drop` implementation. If a user sends SIGINT, or an async task is cancelled, standard execution flow is bypassed. The tracker must implement `Drop` to automatically post `Abandoned` or `Cancelled` to the ledger.

### 4. MIGRATION RISK

**PR Breakdown (§G):** The 7a (rename) and 7b (behavior swap) split is an excellent de-risking strategy. It allows continuous validation of `crates/kaish-kernel/tests/latch_trash_tests.rs`.

**The Hidden Breakages:**
1. **Types:** As noted in the Blockers, `crates/kaish-kernel/src/nonce.rs:46-191` uses a `u32`. If the new ledger uses cryptographic strings, any downstream tool calling `--confirm=12345` or parsing integer responses will panic.
2. **Persistence:** If the new Ledger is in-memory only (like `NonceStore`), a crash loses all `Granted` states. If it flushes to disk synchronously to survive kernel reboots, it introduces massive latency to `gate_overwrites`. The implementation must default to async-flush or in-memory-only with explicit documentation that pending approvals do not survive kernel panics.

### 5. RESOLUTION OF OPEN QUESTIONS

Based on the evidence and operational constraints:

1. **Ring capacity and fail-loud-when-full:** Implement **Partitioned Retention**. Keep an unbounded (or memory-aware) map of *active/pending* request chains, but immediately stream *settled* terminal chains to the `LedgerSink`, removing them from memory. Do not artificially cap and fail-loud active requests, as a heavy auto-approved loop will needlessly crash.
2. **Should `fs.*` auto-grant post when latch is off?** **Yes.** A security ledger must provide non-repudiation. Emitting `Requested` + immediately `Granted` (Policy: Pass-Through) when the latch is off gives auditors a complete trace of what the agent touched.
3. **`approval.request` spanning the wait:** **Use Span Events/Linked Spans.** Do not hold an OpenTelemetry span open for 15 minutes waiting for a human. Start a span for the request, end it, and log the ID. When the decision arrives, start a new span linked to the first. Long-lived spans cause trace drops and backend timeouts.
4. **Should `Irreversible` refuse `--confirm` entirely?** **Context-Dependent Refusal.** Reject `--confirm` (and require out-of-band `Kernel::grant`) if the invocation context is a non-terminal agent (`agent()`). Permit it if the context is an interactive human (`repl()`).

### 6. WHAT'S MISSING & ARCHITECTURAL CRITIQUE

**The "Double-Entry" Framing is Dressing:**
The proposal calls this a "double-entry authorization facility." This is a misnomer that risks confusing future maintainers. True double-entry accounting enforces that `Assets = Liabilities + Equity` via offsetting debits and credits in a single transaction. This design is actually a **Dual-Journal Capability Ledger** or a **Split-Authority Append-Only Log**. It enforces separated posting authorities (`Requester` vs `Approver`), not trial-balance arithmetic. Drop the "double-entry" nomenclature in the code to avoid conceptual friction.

**Operational Defensiveness (Missing Failure Mode):**
The design lacks a specification for `LedgerSink` backpressure. If the external audit sink (e.g., a file or network stream) blocks, does the kernel pause `ToolCtx::request_approval`?
*Recommendation:* The sink must operate asynchronously via an unbounded or heavily buffered channel. If the channel fills, the kernel must explicitly fail-closed (halting agent mutations via `cas_overwrite` in `crates/kaish-kernel/src/tools/context.rs:828-921`) and log a critical error, rather than blocking the main async reactor or silently dropping audit logs.
