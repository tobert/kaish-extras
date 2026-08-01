# Approval-ledger review — gpt-5.6-sol (batch, max thinking), 2026-08-01

Cross-model review of [../approval-ledger.md](../approval-ledger.md) via kaibo
deliberate (`gpt-deliberate` cast; dossier built by gpt-5.6-luna over the kaish
tree + attached design docs). Verbatim below; the adopted amendments were
folded into the design doc's body on 2026-08-01 (revision 2 — see its
Provenance section). Line citations below resolve against revision 1:
`git show 1b36591:docs/design/approval-ledger.md`.

This is the more demanding of the two reviews — it says "do not merge the public
types yet" and identifies six blockers, several of which the gemini pass missed.

---

# Verdict

**Do not merge the public ledger types or `ToolCtx` API yet.** The proposal has the right overall direction—portable approval production, explicit operation/resource identity, typed exit-2 control flow, redemption-time checks, and preservation of background-job behavior—but its core concurrency, capability, and execution-lifecycle contracts are not yet specified tightly enough to implement safely.

The principal blockers are:

1. **The state machine cannot represent multiple or concurrent redemptions.** A redemption needs its own `AttemptId`, and redemption must be a linearizable reservation operation.
2. **Expiry, renewal, revocation, standing-use consumption, and settlement need one explicit transactional ordering model.**
3. **Dispatch-seam settlement cannot handle dropped futures, panics, or process death merely by running code after `tool.execute()` returns.**
4. **The claimed type-level security boundary is achievable only with stricter capability and result types than proposed.** In particular, tokenless public request views must be structural, not a redaction convention.
5. **Replay must carry an internal correlation to the original request.** Otherwise replay either posts a new request or ambiguously redeems the old one.
6. **The migration is not a rename followed by a behavior change.** It changes wire format, nonce reuse, gating fast paths, trash precedence, job lifecycle, and who is allowed to confirm.

A materially simpler **request/grant/attempt table plus append-only audit events** would provide the stated safety properties. "Double-entry" currently adds conceptual weight without a corresponding independently enforced accounting invariant.

---

# Ranked findings

## Blocker 1: The state machine is not sound for more than one redemption

The proposal permits repeated redemption while `max_redemptions` has not been reached and states that every `Redeemed` must have exactly one terminal `Settled` or `Abandoned` successor (`approval-ledger.md:264-299`). But the proposed terminal entries identify only the `RequestId`, not a particular execution attempt (`approval-ledger.md:212-218`).

That makes the balance rule uncheckable when there are two redemptions:

```text
Redeemed(request R)
Redeemed(request R)
Settled(request R)
```

There is no way to determine which redemption was settled, whether one remains outstanding, or whether a later second settlement is a duplicate.

The aggregate state has the same problem. A request cannot simply be in one `Redeeming` state if several executions can be outstanding. Nor is `Granted → Granted: redeem again` sufficient: redemption is not merely a request-state transition; it creates an execution obligation that has its own lifecycle.

The current nonce avoids this issue by having no consumption or settlement model at all. `NonceStore::validate` performs validation under its mutex but deliberately does not consume the nonce (`crates/kaish-kernel/src/nonce.rs:119-155`), and nonce reuse is tested (`nonce.rs:209-217`). Introducing consumption is therefore a semantic change, not just a stronger implementation.

### Required model

Every execution needs a unique attempt:

```rust
struct AttemptId(...);

Redeemed {
    attempt: AttemptId,
    request: RequestId,
    grant: GrantId,
    ...
}

Settled {
    attempt: AttemptId,
    outcome: Outcome,
}

Abandoned {
    attempt: AttemptId,
    reason: AbandonReason,
}
```

The ledger must expose a linearizable operation such as:

```text
redeem(request, credential, expected_conditions) -> AuthorizationAttempt
```

That operation must atomically:

1. establish the authoritative ordering against expiry, revocation, renewal, and other redemption attempts;
2. verify remaining grant uses;
3. reserve or consume one use;
4. append/create the `Redeemed` attempt;
5. return an attempt handle.

Settlement must consume that attempt handle and be idempotent by `AttemptId`.

No individual listed state is clearly unreachable from the available excerpts. The deeper issue is that there is no well-defined total state function once multiple attempts exist, so reachability and balance cannot be proven.

---

## Blocker 2: Concurrency and expiry require an explicit linearization contract

The proposal materializes expiry when first observed and allows renewal after re-observing transitions (`approval-ledger.md:287-297`, `:324-333`). It does not define which operation wins in races such as:

- redemption versus expiry;
- redemption versus revocation;
- old-request redemption versus renewal;
- two kernels consuming the last grant use;
- two observers both appending `Expired`;
- recovery and a late settlement both trying to terminate the same attempt.

A shared `Mutex` could make these correct for an in-memory ledger, but only if all checks and appends occur in a single critical section and all observer-generated events are idempotent. The design currently describes outcomes, not the required atomic ledger operations.

A suitable rule would be:

> An operation wins according to the order in which its conditional ledger transaction commits. A redemption committed before expiry/revocation remains authorized; one committed afterward fails. Every derived event has a uniqueness key, and duplicate expiry, voiding, or abandonment is rejected or treated idempotently.

Standing grants have the same issue. Matching, checking revocation, consuming `max_uses`, and creating the concrete grant must be one transaction. Otherwise two concurrent requests can consume the last standing use.

### Clock assumption

The proposal mentions kernels sharing a ledger (`approval-ledger.md:483-490`) and mixes durable `SystemTime` records with `Instant`-style expiry (`:229`). If "shared" means several kernels in one process using the same `Arc<LedgerInner>`, one monotonic clock domain is adequate. If it includes separate processes, persistence, or restart, process-local `Instant` values are not meaningful across those boundaries.

The document should explicitly choose one:

- **Initial implementation:** in-process only, with a ledger-owned monotonic clock and no durability claim.
- **Durable/cross-process implementation:** absolute expiry plus defined wall-clock rollback behavior, or a persisted lease/epoch mechanism.

Without that scope statement, the cross-kernel concurrency claim is too broad.

---

## Blocker 3: "Settle after `tool.execute()`" does not cover cancellation or failure

The proposal places auto-settlement at the dispatch seam after `tool.execute()` returns (`approval-ledger.md:339-351`). The current seam does capture invocation immediately before execution (`crates/kaish-kernel/src/kernel.rs:3295-3324`), but post-execution code only runs if the future returns normally (`kernel.rs:3324-3340`).

It does not run when:

- the future is dropped;
- its task is aborted;
- execution panics;
- the process terminates;
- a nested dispatch outlives the outer invocation;
- the side effect happens and cancellation occurs before the tool returns.

The current cancellation API is cooperative and token-based (`crates/kaish-tool-api/src/ctx.rs:82-101`); it does not provide a dropped-future callback. Therefore the claim that a dropped future automatically settles as `Cancelled` is not implementable at the stated "after return" point.

There is also an outcome-honesty problem: cancellation does not prove that no effect occurred. A tool may perform the write and then be cancelled before reporting success. Recording only `Cancelled` could misrepresent the audit history.

### Required execution protocol

Install a dispatcher-owned attempt guard before entering the mutation:

- Normal return explicitly settles with the exit outcome.
- Panic/drop performs best-effort termination through a synchronous queue or outbox.
- Process death leaves an open leased attempt; recovery marks it `Abandoned` or preferably `Outcome::Unknown` only after lease expiry.
- A late settlement and recovery abandonment compete through a conditional transition on the same `AttemptId`.

`Drop` cannot reliably perform arbitrary async I/O, so a durable implementation needs either a local synchronous journal or a recovery protocol. "Abandoned" should not imply "no write happened"; an `Unknown` or `LostExecutor` outcome is more accurate.

Background jobs need the same explicit lifecycle. Today, `JobStatus::Latched` is derived from a completed `ExecResult` (`crates/kaish-kernel/src/scheduler/job.rs:180-211`), while `Job::latch()` merely clones and stamps that request (`job.rs:214-230`). There is currently no job-owned active-redemption guard to reuse.

---

## Blocker 4: Replay is coherent only if it is internally correlated to the original request

Current `Kernel::confirm` prepends a nonce to captured argv and re-dispatches it (`crates/kaish-kernel/src/kernel.rs:1594-1607`). It fails if no exact invocation was captured and retires a latched background job after successful replay (`kernel.rs:1595-1616`).

Under a request-specific grant, ordinary replay will reach the approval site again. The proposal must define why that site redeems the old request instead of posting a new one.

A coherent protocol would be:

1. The original request stores an immutable operation/resource description and invocation-capture status.
2. An authority grants that request.
3. `Kernel::confirm` atomically reserves an attempt for the original request.
4. Replay is dispatched with an internal `RedemptionContext { request_id, attempt_id }`, not merely a public bearer string.
5. At the gate, the newly constructed draft must match the granted operation and resources exactly before the attempt is accepted.
6. Any subsequent, different gate in the replay posts a new request.

This also makes concurrent shared-kernel replay coherent: only the ledger transaction can reserve a permitted use. Redemption-time resource checks should validate the originally approved transition, while the actual git or filesystem mutation must still use an atomic conditional write.

Invocation capture is currently best-effort: `tool_args.to_argv().unwrap_or_default()` silently substitutes an empty argv (`crates/kaish-kernel/src/kernel.rs:3310-3321`). Direct tool execution also bypasses capture. The ledger should represent distinct states such as `Exact`, `Unavailable`, `CaptureFailed`, and `DirectExecution`; only `Exact` should be replayable through `Kernel::confirm`.

---

## Blocker 5: The security boundary is not yet structural

The proposal's intended boundary is reasonable under its stated exclusion of malicious in-process code: a `Requester` that cannot invoke grant methods is useful least privilege. Two private wrappers around the same `Arc<LedgerInner>` can enforce that distinction in safe Rust if their constructors and methods are controlled.

However, the current proposed capability graph does not yet prove the stronger claim that a gated agent cannot obtain a grant token or invoke approval authority.

### Concrete escape paths that must be closed

1. **Tool-facing reads must never return token-bearing objects.**
   The proposal gives tools an approval view while also describing `Approvals::get(id)` as exposing a grant to the embedder (`approval-ledger.md:452-455`, `:518-520`). Those must be separate interfaces and result types.

2. **Public results must be tokenless by construction.**
   The current latch is created inside `ExecContext::latch_result` (`crates/kaish-kernel/src/tools/context.rs:759-798`). `Job::latch()` only later stamps a job ID (`crates/kaish-kernel/src/scheduler/job.rs:223-230`), and foreground results never pass through that method. Therefore `Job::latch()` is not a universal redaction chokepoint.

   `ExecResult` is also a serializable control-plane type whose latch survives result transformations (`crates/kaish-types/src/result.rs:58-71`, `:195-208`, `:483-500`). A token that ever enters the public request value can leak through clones, JSON, VFS, telemetry, foreground results, or custom execution paths.

   The fix is a distinct `ApprovalRequestView` that contains no credential field at all.

3. **Authority should be a capability object, not a boolean.**
   `Kernel::grant`, `Kernel::confirm`, and `with_approval_authority(bool)` on the same general object do not establish type-level separation (`approval-ledger.md:493-500`). The host should possess a non-constructible `ApproverHandle`; an agent-facing kernel/session should not.

4. **No ordinary shell builtin may bridge to that capability.**
   The dossier does not establish which kernel methods are shell-reachable. If an agent can invoke a builtin that calls `Kernel::grant`, the separation is defeated even if `ToolCtx` itself only exposes `Requester`.

5. **Downcasting limits the threat claim.**
   `ToolCtx::as_any_mut` allows trusted tools to recover the concrete context (`crates/kaish-tool-api/src/ctx.rs:106-121`). That is acceptable only because malicious in-process tools are explicitly out of scope. The proposal should describe the guarantee as protection against command-level agents and ordinary portable tools, not hostile loaded Rust code or a hostile embedder.

The strongest design is to avoid delivering a bearer token to requesters at all. The authority grants by request ID; the kernel holds any internal redemption credential and binds it to the principal/session. The requester may trigger replay after approval, but cannot create the approval.

---

# API design review

## `ToolCtx::request_approval`: fail closed is correct, but the exact trait API matters

Returning exit 2 when a context lacks ledger support is the right default for a write-capable plugin (`approval-ledger.md:426-462`). Proceeding in a minimal test context would be unsafe.

It is nevertheless a behavioral migration:

- existing test contexts will now reject writes;
- direct tool tests need an explicit approving fixture;
- out-of-tree implementations may compile but change behavior;
- adding an async method to `&mut dyn ToolCtx` requires a defined object-safe shape.

The current portable trait is mostly synchronous (`crates/kaish-tool-api/src/ctx.rs:43-104`). The proposal should specify whether it uses `async_trait` or an explicit boxed future. It should also decide whether `request_approval` merely posts and returns `Pending`, or waits for a human decision. Holding `&mut ToolCtx` across an indefinite approval wait can prevent other context work and complicate cancellation.

Prefer a result that distinguishes:

```text
Authorized(AttemptHandle)
Pending(TokenlessRequestView)
Denied(Reason)
Unsupported
LedgerUnavailable
```

All non-authorized outcomes must fail closed.

## Draft-then-stamp is good

Having the plugin construct an `ApprovalDraft` while the kernel stamps principal, invocation, context, timestamp, and TTL is a sound separation (`approval-ledger.md:464-477`).

Two constraints are necessary:

- A draft must not deserialize as or be accepted as a stamped request.
- Stamping proves provenance of the metadata, not truth of the plugin-supplied resource description. Resource-specific code must canonicalize and validate repository identity, paths, refs, and expected transitions.

## Approver decision chain: never hold the ledger lock across external work

The four-stage decision chain can work if its transitions are explicit:

- `Matched/Grant` commits a grant.
- `Denied` is terminal.
- `NotMatched` alone allows the next stage.
- Hook errors and timeouts fail closed rather than becoming `NotMatched`.

No resolver, embedder hook, human prompt, telemetry call, or async wait may run while holding the ledger mutex. The safe pattern is:

1. snapshot request and version under lock;
2. release lock;
3. run the external stage;
4. conditionally commit against the same version/state;
5. retry or reject if the state changed.

That is especially important for "transition conditioned on not matched": a stale `NotMatched` result must not overwrite a concurrent grant, void, expiry, or denial.

## Standing grants: all-or-nothing is the right default

Requiring every requested resource to match is safer than partial matching (`approval-ledger.md:398-420`). The proposal still needs to define:

- set versus multiset semantics and duplicate resources;
- whether one pattern may match several resources;
- canonicalization before matching;
- precedence among multiple matching rules;
- atomic `max_uses` consumption;
- revocation racing with use;
- whether broad string globs are permitted for typed resources.

Typed resource matchers are preferable to generic textual globbing, particularly for git repository and ref identities.

## Redemption checks are useful but do not close the final TOCTOU window

The proposal correctly generalizes the current compare-before-write behavior. Current `cas_overwrite` re-reads and rejects mismatch, but explicitly is not OS-atomic (`crates/kaish-kernel/src/tools/context.rs:260-292`).

A ledger resolver check says that the expected condition held at authorization time. It does not prevent another actor changing the resource before the actual write. For git refs, the final mutation must use git's compare-and-update primitive. The documentation should say "detects stale authorization and supplies an expected value to an atomic conditional mutation," not "closes TOCTOU" by itself.

---

# Migration risk

## The latch-off mapping is a substantive behavior change

Today:

- trash takes precedence over latch;
- latch applies only where trash cannot protect the mutation;
- both disabled means proceed;
- new-file and append cases may proceed without a gate  (`crates/kaish-kernel/src/tools/context.rs:223-257`);
- `gate_overwrites` has a direct no-trash/no-latch fast path (`context.rs:835-840`);
- trash snapshot/CAS handling has its own path (`context.rs:907-951`).

Changing latch-off to:

```text
Requested → Granted{Policy} → Redeemed → Settled
```

means those fast paths no longer exist. It also raises unanswered questions about trash-protected writes, append/new-file operations, unconditional `trash empty`, and whether ledger failure blocks formerly ungated operations.

Before migration, write an explicit operation matrix covering:

- operation class;
- trash enabled/disabled;
- approval enabled/disabled;
- reversible versus irreversible;
- foreground/background/direct execution;
- expected ledger events;
- failure behavior.

The invariant that trash failure is loud and never falls through to an unprotected overwrite must remain unchanged.

## PR 7a/7b is not a safe rename-then-semantics split

`ExecResult.latch` to `ExecResult.approval` changes Rust APIs and JSON keys. The new request also changes shape, IDs, principal metadata, resources, invocation representation, and token visibility. Current `LatchRequest` rejects unknown fields and is serialized directly (`crates/kaish-types/src/result.rs:72-74`, `:195-208`).

Thus "byte-identical modulo key rename" is possible only for a temporary compatibility projection, not for the final ledger type.

A safer sequence is:

1. Formalize ledger transactions and model-test concurrency.
2. Add internal approval types alongside the latch types.
3. Implement a latch compatibility adapter backed by the ledger, preserving current TTL/reuse semantics.
4. Port one gate while retaining existing external `ExecResult.latch`.
5. Validate foreground, pipeline, background, VFS, trash, and CAS behavior.
6. Introduce the new tokenless serialized API as an explicit wire-format break.
7. Remove the compatibility layer later.

## What can actually remain stable

- **Exit code 2:** straightforward to preserve.
- **`--confirm` spelling:** can remain accepted.
- **`Kernel::confirm` name:** can remain as a façade.

Their current semantics cannot all remain unchanged while also withholding bearer credentials from agents. Current confirmation replays captured argv using the request's nonce (`crates/kaish-kernel/src/kernel.rs:1594-1607`), and current nonce validation is reusable. A one-use, authority-bound grant is observably different.

The compatibility façade should document:

- who may call it;
- whether it consumes a use;
- how it correlates replay to the old request;
- what happens when invocation capture is absent;
- whether retries after failed execution remain allowed.

## Background contracts need explicit preservation tests

The migration must retain the behaviors covered in `latch_trash_tests.rs`, including:

- pipeline latch precedence (`:370`);
- background confirmation (`:1173`);
- retirement after successful confirmation (`:1202`);
- distinct latched status (`:1265`);
- VFS surfacing (`:1312`);
- cleanup preservation (`:1334`);
- kill refusal without discard (`:1369`).

Renewal also needs a job-to-request index. If renewal creates a new request ID, the job's stored result, status, confirmation lookup, and discard behavior must all move to that new ID consistently.

---

# Recommendations on the four open questions

## 1. Ring capacity and retention

**Use separate operational and audit retention.**

Maintain:

- a bounded in-memory index for live requests, grants, and attempts;
- a separate append sink or durable audit store for completed history.

Live entries should not be evicted. If capacity is exhausted by live entries, new privileged operations should fail closed, but the system needs quotas, metrics, and administrative cleanup to prevent trivial availability denial.

If the first release is memory-only, call it an operational ledger, not a durable audit ledger. Evicting settled chains conflicts with the promise to answer "what did this agent actually delete?"

## 2. Post `fs.*` auto-grants when latch is off

**Do not make this the initial migration default.**

Initially, preserve current latch-off fast paths and post requests only where the existing latch would gate. If comprehensive auditing is wanted, add a separate effect/audit event for successful destructive operations.

After the authorization lifecycle is proven, full policy auto-grants can be introduced as a documented behavioral and performance change.

## 3. Long-lived `approval.request` spans

**Use linked short spans rather than one minutes-long span.**

Track separately:

- request creation;
- request-to-decision latency;
- approver/hook decision duration;
- redemption-to-settlement duration.

Link them with `RequestId`, `AttemptId`, and trace context. Long-lived spans can retain collector resources and produce misleading operation latency.

## 4. Out-of-band approval for `Irreversible`

**Yes. Require an approver capability, not possession of a bearer token.**

A human REPL may remain an approval authority and keep the `--confirm` user experience, but a token visible in an agent's result must not itself authorize an irreversible operation.

For irreversible requests, bind approval to:

- an authority capability;
- the request;
- the principal/session;
- exact operation/resources;
- a one-use attempt reservation.

---

# Missing operational and security requirements

The proposal should add explicit policies for:

1. **Ledger failure before a write:** fail closed.
2. **Settlement failure after a successful write:** record through a durable outbox or report an audit gap; rollback is generally impossible.
3. **Process crash:** open attempts become unknown after a lease, not proven unsuccessful.
4. **Backpressure and denial of service:** per-principal pending limits, live-entry quotas, metrics, and administration.
5. **Token handling:** cryptographically random credentials, hashes at rest where possible, no token in logs, telemetry, JSON, VFS, or debug formatting.
6. **Principal provenance and tenant isolation:** shared ledgers need namespaced identities and session binding.
7. **Resource canonicalization:** repository identity, worktree identity, path symlinks, ref normalization, encoding, and case sensitivity.
8. **Partial multi-resource effects:** one approved request may mutate some resources before another fails; settlement should not imply atomic completion.
9. **Idempotency:** retries and duplicate dispatcher calls need stable operation/attempt IDs.
10. **Privacy and retention:** captured argv and resource names may contain secrets.
11. **Corruption and integrity:** an append-only log is not automatically tamper-evident; if adversarial audit integrity is a goal, it needs authenticated storage or signatures.
12. **Lock discipline:** no I/O, hooks, callbacks, resolvers, telemetry, or `.await` while holding the ledger lock.

---

# Is "double-entry" the right abstraction?

Not yet.

The useful mechanism described is an authorization state store with provenance:

```text
Request
Grant
ExecutionAttempt
Settlement
```

plus append-only audit events. That is enough to enforce:

- requester/approver capability separation;
- bounded grant use;
- expiry and revocation;
- exact operation/resource matching;
- precondition verification;
- replay correlation;
- execution outcome recording.

"Double-entry" would be justified if the implementation enforced genuinely independent posting authorities and a checkable balancing relationship. As proposed, both sides ultimately wrap the same ledger, the kernel can own both APIs, and the terminal events cannot currently be balanced against individual redemptions.

I recommend naming it simply the **approval ledger** and implementing a normalized authoritative state table with transactional audit events. If the project later adds independently signed requester and approver records, the stronger double-entry analogy can be restored.

In short: **keep the safety goals, simplify the framing, and formalize request/grant/attempt transactions before exposing the API or starting the git writer.**
