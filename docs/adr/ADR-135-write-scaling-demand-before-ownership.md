# ADR-135: Scale SQLite writes by reducing writer demand before changing ownership

- Status: accepted (2026-08-01)
- Date: 2026-07-30

## Context

khive uses SQLite in WAL mode as its embedded primary store. WAL permits concurrent readers, but one WAL file admits exactly one writer transaction at a time. The capacity knee is therefore not a client-count threshold. It is the utilization and variance of the single writer:

`ρ = λ_request × E[D_writer per request]`

As `ρ` approaches one, queueing delay grows non-linearly; service-time variance makes the tail worse. A deployment may support hundreds of connected clients when their aggregate writer demand remains below the measured service rate, but it cannot execute hundreds of simultaneous writes against one mainline SQLite WAL file. A full write-path census of the verb surface (every request verb classified by writer acquisition; zero read-only verbs found) is the measured ground truth for current behavior.

Writer demand is amplified above the semantic operation count. A new-root `comm.send` performs `6 + 2M` separate write transactions, where `M` is the number of selected embedding models. `memory.remember` performs `4 + M + A`, where `A` is the number of annotation targets. Audit normally uses the same `khive.db` and is best-effort on allowed paths, but its attempted append still consumes the same exclusive writer privilege. Reducing `E[D_writer per request]` is consequently the first capacity lever, independent of writer topology.

The current optional `WriterTask`, introduced as Component A by ADR-067, is instantiated once per `ConnectionPool` per process. It is off by default behind `KHIVE_WRITE_QUEUE`. It is not a cross-process owner, has no post-startup file-access fence, and does not batch requests across commits. Direct paths remain: `SqlBridge::writer` opens a standalone writer before checking for a queue handle, and `with_writer_unmanaged` bypasses the queue unconditionally. An unenforced owner is not an owner.

The prominent caller-visible five-second failure is a **pool writer-mutex checkout timeout before SQLite executes any statement**. It is not SQLite's 30-second `busy_timeout`. It is a checkout timeout wearing a database-timeout costume, and the current wording misdirects incident diagnosis toward WAL locks and checkpointing. Queue admission, pool checkout, `BEGIN IMMEDIATE`, DML, and commit are distinct stages and must remain distinct in errors and metrics.

The pooled writer uses `synchronous=NORMAL` and retains `wal_autocheckpoint=4000` even though checkpointing also has a dedicated connection. Ordinary checkpoint work uses PASSIVE; TRUNCATE escalation temporarily applies a two-second busy policy. A dedicated checkpoint connection therefore does not prove that checkpoint work is absent from application commit paths.

The bundled SQLite version is 3.48.0, established from the vendored `libsqlite3-sys-0.31.0` source. It falls within the upstream WAL-reset-corruption affected range through 3.51.2. This fact does not explain the observed five-second checkout timeouts. It does require a correctness version gate before concurrency experiments can produce an architectural verdict.

ADR-007 remains in force: a namespace is attribution, not a physical isolation or capacity boundary. A namespace column in one file does not create another writer. Physical files can scale aggregate capacity across tenants, subject to hot-tenant and shared-device limits, but the tenancy and routing decisions required for that topology are outside this ADR.

ADR-067 Component A remains the description of the present process-local `WriterTask` slice. This ADR extends it by defining the conditions under which queue routing may become mandatory and by deferring its Component B cross-request batching design until measurement justifies it. It does not silently reinterpret Component A as cross-process ownership.

Compatibility constrains rollout. In the first shipped slice, absent environment variables retain byte-identical behavior. Routing, overload, scheduling, acknowledgement, and error-semantic changes are introduced behind explicit flags or in a major version.

## Relationship to ADR-131

ADR-131 (batch write admission control, accepted) governs admission for every write call in the serialization domain. This ADR amends it in exactly two places and preserves the rest. Where the two texts differ and no amendment is declared here, ADR-131 controls.

**Amendment 1 (to ADR-131 Decision 1): default queue enablement is deferred until routing is strict.** ADR-131 Decision 1 sets `PoolConfig::write_queue_enabled` to default `true` for batch-surface-serving deployments. That default assumed queue routing was complete. The census recorded in Context shows it is not: `SqlBridge::writer` opens a standalone writer before checking for a queue handle, and `with_writer_unmanaged` bypasses the queue unconditionally. A default-on queue beside live bypass paths asserts a single-admission property the code does not have, and the admission metrics of ADR-131 Decision 4 would undercount real writer demand by exactly the bypassed share. F2 therefore defers the default flip until its five strict-routing conditions hold and one release of production-representative evidence shows no direct-writer violations. ADR-131's admission contract (Decisions 2 through 5) remains binding and unchanged for every queue-enabled deployment in the interim; only the default in Decision 1 moves.

**Amendment 2 (to ADR-131 Decisions 3 and 4): class-weighted service is a deferred, conditional amendment; until it is accepted, the single global rotation stands unchanged.** ADR-131 Decision 3's single call-aware round-robin over every pending call in the serialization domain, its per-operation rotation-wait bound, and Decision 4's `writer_admission_scheduler_wait_seconds` semantics all remain in force exactly as written. F5's reserved capacity and weighted service for interactive semantic writes are a design intention, not an accepted scheduling contract: adopting them requires a dedicated amendment to Decisions 3 and 4 that defines the class-selector algorithm, numeric weights and the minimum bulk service share, a measurable bulk queue-age or turn bound replacing the single-rotation wait bound for cross-class waits, and the class-aware replacement semantics for the scheduler-wait metric. No implementation may ship class weighting before that amendment is accepted. F5's class taxonomy (interactive semantic, bulk, best-effort audit/telemetry) may be introduced earlier for classification and observability only, without affecting admission order. If class weighting is adopted and its kill condition later fires, scheduling collapses back into ADR-131 Decision 3's single global call-aware rotation, not into global FIFO (which ADR-131 rejected and this ADR also rejects).

## Decision

### F1. Write-ownership topology

We will not describe the current per-process `WriterTask` as ownership of a SQLite file. For the first slice, khive keeps the existing multi-process contention topology and makes its stages observable. A later cross-process-owner slice is approved in principle only if it provides both exclusive routing and enforceable fencing.

A true owner requires:

1. all post-startup write-bearing operations to route through one daemon endpoint;
2. removal or denial of direct writable opens outside that owner, including SQL bridge and unmanaged store paths;
3. owner-generation identity on admission, execution, and diagnostics;
4. OS-enforced writable-file isolation, such as filesystem permissions or a separately held descriptor/capability unavailable to clients, so a stale same-user process cannot simply ignore an advisory lease; and
5. failover that transfers the writable capability only after the old owner can no longer access it.

An advisory lock, heartbeat, PID file, lease timestamp, or path-identity check may detect mistakes, but none fences a stale process that can still open the database read-write. If deployment cannot provide enforceable writable-file isolation, the architecture remains explicitly multi-writer-process SQLite with busy handling; it must not claim single ownership.

Strongest case against this decision: implementing measurement before ownership prolongs decentralized lock races and retry amplification, while a single daemon queue would immediately centralize admission and diagnosis. This objection flips the decision when a route inventory proves every writer can be proxied, an adversarial stale process is unable to write after ownership transfer, and owner failure/restart tests preserve acknowledgement and idempotency contracts. At that point, cross-process ownership becomes the default topology.

### F2. Queue-on-by-default and bypasses

`KHIVE_WRITE_QUEUE` remains off by default in the compatibility slice. This defers ADR-131 Decision 1's default-on setting; see "Relationship to ADR-131", Amendment 1. Enabling it continues to be explicit. Before any default flip, queue-enabled mode must become strict:

- `SqlBridge::writer` must check and route through the queue before opening a standalone writer;
- `with_writer_unmanaged` must be removed from runtime request paths or replaced with an owner-executed top-level/atomic operation;
- queue spawn or runtime failure must fail closed for writes in strict mode rather than silently falling back;
- every direct writer violation must be observable and test-failing; and
- startup, migrations, checkpointing, recovery, and top-level maintenance must be classified explicitly rather than hidden as exceptions.

Once strict routing is complete, an explicit `KHIVE_WRITE_ROUTING=strict` mode will exercise it without changing absent-variable behavior. Default enablement requires one release of production-representative evidence showing no direct-writer violations and no regression in completion or recovery semantics; otherwise it waits for a major version.

Strongest case against this decision: leaving the queue off preserves the very pool-mutex collisions it was built to eliminate and delays useful bounded backpressure. The recommendation flips if an A/B control shows queue-on materially lowers retry attempts or caller errors without hidden fallbacks, while a route audit and fault injection prove that all accepted writes retain their result semantics. Merely showing a lower local queue depth is insufficient.

### F3. Cross-request batching

ADR-067 Component B will be built only after measurement, not in the first scaling slice and not rejected permanently. `synchronous=NORMAL` makes the comfortable fsync-amortization story unproven. We will first measure transaction fixed cost `F`, per-operation variable cost `v`, achieved batch fill, lock-hold duration, and interactive tail latency.

Component B is authorized when, under the production-representative workload:

- measured fixed transaction cost is at least 20% of unbatched writer residence for a material write class;
- batch size 8 improves sustainable useful throughput by at least 20% over batch size 1;
- interactive p99 remains within its SLO and does not regress by more than the allocated writer-latency budget;
- replies are withheld until the outer commit completes;
- each independent request has a savepoint or another documented isolation rule;
- commit failure and outcome-unknown are returned to every affected request; and
- operation count is supplemented by byte/page/work and maximum lock-hold limits.

Batch sizes 1, 8, 16, and 64 will be tested, but no illustrative gain becomes a product claim until observed. The collection window and maxima will be selected from measured fill and latency distributions, not copied from an earlier proposal.

Strongest case against this decision: batching is the only proposed single-file mechanism capable of increasing the intrinsic service rate, so deferral may optimize transaction count while leaving the main fixed cost untouched. The decision flips to “build now” if instrumentation already demonstrates a dominant transaction-level `F`, adequate batch fill, and a modeled batch-8 gain above the threshold. It flips to “reject” if `F` is below 10%, batch-8 throughput gain is below 10% in two representative runs, or meeting throughput requires a collection delay or lock hold that violates interactive SLO.

### F4. Reduce writer residence per request

The first implementation priority is operation-local transaction coalescing. This changes neither cross-request ordering nor acknowledgement semantics and reduces both transaction count and queueing variance.

For new-root `comm.send`, the outbound note row, its FTS projection, its vector projections, the root thread-id patch, the inbound note row, its FTS projection, and its vector projections will execute in one operation-scoped transaction. The dispatch audit remains outside that atomic semantic transaction while it retains best-effort semantics. The target is to reduce `6 + 2M` transactions to one semantic transaction plus, while still configured, one independent best-effort audit transaction. A supplied-thread-id send has the same target without the root patch.

The transaction must perform all database work only. Embedding computation and other external or suspendable work must be completed before `BEGIN IMMEDIATE`, with validated results passed into the transaction. Failure of any semantic projection rolls back the message operation; no caller receives a partial success.

The same pattern applies to the note/projection/annotation portion of `memory.remember`, reducing its semantic writes to one transaction where schema and store contracts permit. Best-effort `NoteCreated` telemetry and dispatch audit are not folded into the semantic transaction unless their product contract is explicitly changed from best-effort to atomic.

`comm.read` remains a semantic write. It may be collected with other receipts only if every reply is withheld until the outer commit and the implementation preserves per-message authorization, whole-properties concurrency protection, unread membership, redelivery behavior, and `created_at` ordering. Acknowledging before a later sweep commit is rejected.

Best-effort telemetry, including `RecallExecuted`, stays off the caller's synchronous semantic path. Best-effort dispatch audit may move to a separate bounded asynchronous channel, but only behind an explicit flag with declared crash-loss, overflow, ordering, shutdown-drain, and visibility semantics. Compliance-required audit must not be silently shed; it requires a separate fail-closed contract.

Strongest case against this decision: one large operation transaction can hold the writer longer, correlate projection failure with the semantic mutation, increase WAL frames, and make retries more expensive. It also removes partial projection recovery that callers may have implicitly tolerated. The decision flips for a particular coalescing boundary if fault-injection finds a required partial-success contract, if p99 writer hold time rises enough to worsen interactive queue age despite lower transaction count, or if external computation cannot be kept outside the transaction. The evidence must be per operation; it does not reverse coalescing globally.

### F5. Queue-full overload policy

Strict queue mode rejects overload with a stable typed result under ADR-131 Decision 2's existing admission contract: an operation that cannot obtain channel capacity waits only through its finite admission deadline and remaining-request budget, then returns the saturation result defined there. It does not block without a bound, and it does not reject before that admission window has run. Accepted work continues to completion even if the caller disconnects; cancellation after admission is not implied.

Reserved admission capacity and weighted service for interactive semantic writes relative to bulk work are a deferred, conditional amendment to ADR-131 Decisions 3 and 4: until that amendment is accepted with a concrete selector algorithm, numeric shares, and class-aware metric semantics, ADR-131's single global call-aware rotation governs admission order unchanged, and the write classes below serve classification and observability only (see "Relationship to ADR-131", Amendment 2). FIFO is preserved within a semantic ordering key, such as a conversation or explicitly atomic client batch, but strict global FIFO is not required. Best-effort audit and telemetry use a separate bounded class and are shed first according to their declared loss budget. Compliance-required work is never reclassified as best-effort by overload machinery.

The caller receives one of: rejected-before-admission, accepted-and-pending, committed, failed-before-commit, or outcome-unknown. A durable operation handle is deferred until accepted writes can outlive ordinary caller deadlines or owner restarts; if implemented, its idempotency record must commit consistently with the mutation.

Kill condition: if class-weighted scheduling is adopted and adversarial tests then show ordering-key violations, starvation of admitted bulk work beyond its SLO, or interactive p99 improvement below 10% at the cost of more than a 10% reduction in useful throughput, scheduling collapses back into ADR-131 Decision 3's single global call-aware rotation (not global FIFO). Conversely, durable handles become required if more than 0.1% of admitted writes outlive caller deadlines or any accepted write can survive owner restart without a recoverable caller outcome.

Strongest case against this decision: rejection pushes retries to clients and can create a synchronized retry storm; priority makes behavior less predictable and may starve bulk work. Blocking could absorb short bursts with simpler semantics, and strict FIFO is easier to reason about. The recommendation flips to bounded blocking when measured bursts drain within the caller deadline at p99 and retry amplification from rejection exceeds the queueing cost. It flips to strict FIFO when product semantics require global completion order and that requirement is demonstrated by a consumer, not assumed from arrival order.

### F6. Error taxonomy

Caller-visible and metric error stages will be split. Rendered messages may retain compatibility text during the first slice, but structured fields will identify:

- `writer_pool_checkout_timeout`: no pooled writer mutex was obtained; SQLite did not execute;
- `writer_queue_saturated`: the request was not accepted within its admission deadline (ADR-131 Decision 2's typed result, unchanged; the stage field records where in admission it failed);
- `sqlite_begin_busy`: `BEGIN IMMEDIATE` failed or exceeded its stage deadline;
- `sqlite_statement_failure`: DML failed after writer acquisition;
- `sqlite_commit_failure`: commit returned a definite failure;
- `write_outcome_unknown`: commit may have occurred but the caller cannot establish it; and
- `writer_owner_unavailable` or `writer_route_violation`: strict routing could not reach the owner or detected a bypass.

Every native SQLite failure preserves primary and extended result codes through storage, runtime, and wire wrappers. Non-SQLite stages do not invent `SQLITE_BUSY`. In particular, `comm.read` must stop formatting a typed storage error into `RuntimeError::Internal(String)`.

The five-second pool checkout error will be renamed and documented prominently as occurring before SQLite execution. Metrics separately time admission, checkout, `BEGIN IMMEDIATE`, transaction body, and commit.

Implementation note (2026-08-02, first observability slice): the typed
`WriterPoolCheckoutTimeout` source is retained through storage and runtime wrappers. MCP serializes
it with stable `code` and `stage` values of `writer_pool_checkout_timeout`, plus `timeout_ms`,
`capability`, and `operation`; `message` retains the compatibility rendering. The same slice's
diagnostics report an aggregate writer-acquisition count together with pooled, per-operation
standalone, and writer-task connection classes. The PASSIVE diagnostics connection and the writer
task's one-time lifetime connection are infrastructure opens and do not enter that traffic total.
This is counter-level observability only; it does not claim the full per-attempt timing matrix in
F7 is implemented.

Strongest case against this decision: expanding public error variants creates compatibility work and invites callers to bind to low-level implementation stages. The decision flips only if a stable higher-level taxonomy can distinguish safe retry, unsafe retry, overload, and outcome-unknown without losing the stage needed for incident attribution. It does not flip to preserving the current misleading string. During compatibility rollout, old codes may remain as aliases while structured stage fields are added.

### F7. Experiments and decision gates

The measurement annex is mandatory before changing defaults or implementing Component B. It uses scheduled open-loop arrivals so blocked clients do not suppress the load that would have encountered the worst delay. A closed-loop run is retained only as an A/A or comparison control.

Before any concurrency result is accepted, the runtime must report `sqlite_version()`, `sqlite_source_id()`, and compile options. The test refuses an architectural verdict unless the build is on an upstream-fixed release, an applicable fixed backport, or a documented patched build. SQLite 3.48.0 results may be used to validate instrumentation but not to qualify a production concurrency topology. This version gate addresses correctness risk; it is not an explanation for pool checkout timeouts.

The minimal matrix is:

- A/A runs of the current topology to establish harness noise and reproducibility;
- current direct/per-process topology versus strict per-process queue routing, with identical work;
- one process versus multiple processes;
- transaction shapes for audit append, read receipt, one-row semantic mutation, coalesced `comm.send`, and bulk ingest;
- batch sizes 1, 8, 16, and 64 under `synchronous=NORMAL`, with `FULL` as a diagnostic contrast;
- stable rates across the knee plus simultaneous bursts;
- current `wal_autocheckpoint=4000` versus explicitly disabled application autocheckpoint while the dedicated checkpoint task runs; and
- PASSIVE ordinary checkpointing plus a controlled TRUNCATE escalation.

Each writer attempt records request and operation identity, process and connection class, database file identity, scheduled arrival, admission interval, pool checkout interval, `BEGIN` interval, DML interval, commit interval, primary and extended SQLite codes, statement/row/byte/page or WAL-frame work, queue backlog and oldest age, in-service age, batch size and fill reason, retry parent, caller outcome, and best-effort audit outcome.

The experiments produce:

- fixed cost `F` and variable cost `v` by write shape;
- mean, coefficient of variation, p95, and p99 writer service time;
- writer transactions and writer residence per request;
- sustainable useful throughput and retry amplification;
- batch gain and collection-latency cost;
- checkpoint coupling at commit;
- the utilization and burst-drain knee; and
- direct-writer violations.

The single-file design is outside its design envelope when, after operation-local coalescing and any measurement-authorized batching, a production-representative open-loop workload causes any of the following on the hottest file: sustained writer utilization above 0.70 for latency-sensitive traffic, p99 queue age above the allocated writer portion of the SLO, writer-stage errors above the product error budget, or burst drain beyond the earliest caller deadline. These are policy thresholds, not claims of a universal SQLite constant.

Strongest case against this decision: the matrix is expensive, version gating delays urgently needed relief, and instrumentation can perturb short transactions. The decision flips to a smaller experiment only if an A/A control establishes the omitted dimensions as immaterial and the smaller test still identifies `F`, `v`, variance, batch gain, checkpoint coupling, and the knee. It never flips to closed-loop-only testing or to accepting results from an unqualified SQLite build.

## Consequences

The first shipped work attacks the dominant capacity term directly: transaction amplification and unclassified waiting. It does not promise that a queue raises SQLite's intrinsic throughput. It preserves absent-variable behavior while providing explicit strict-routing and asynchronous-best-effort modes for controlled rollout.

Operation-local coalescing makes semantic writes more atomic. It reduces lock acquisitions but can lengthen each lock hold and correlate failures, so per-shape hold-time and rollback tests become release requirements.

The system gains an honest overload surface. Rejection, admission, execution, commit, and outcome-unknown become distinguishable, which reduces unsafe retries and incident misdiagnosis. Existing consumers may need to migrate from string matching to structured error fields.

Per-process queueing remains an optimization rather than a claim of file ownership. A real cross-process owner remains available as a later topology, but only with a fence that stale writers cannot ignore. This may require deployment packaging or privilege separation beyond library changes.

Audit and telemetry acquire explicit product contracts. Moving best-effort work off the response path improves interactive capacity but permits bounded delay or loss. Compliance-required audit cannot use that path without a separate decision.

Component B batching is neither blessed by arithmetic nor rejected by caution. It has numeric entry and kill conditions. Under `synchronous=NORMAL`, its gain may be small; if measurement finds a large fixed cost, it becomes the next single-file capacity step.

The pooled writer's active autocheckpoint is treated as a measured variable, not assumed harmless because a dedicated checkpoint connection exists.

This ADR does not make a client-count promise. Capacity claims are stated in writer demand, utilization, variance, queue age, and burst drain.

## Alternatives rejected, with reasons

### Flip the existing queue on and call it a write owner

Rejected. The queue is once per pool per process, direct writer paths remain, spawn degradation can fall back, and no post-startup cross-process fence exists. This improves some intra-process contention but does not establish one owner per file. It becomes viable only after strict routing and the F1 fencing tests pass.

### Keep the status quo and document timeouts as expected SQLite behavior

Rejected. The visible five-second failure occurs at a Rust-side pool mutex before SQLite executes. Treating it as an ordinary database busy timeout preserves misleading diagnostics, hides transaction amplification, and provides no overload contract. Status quo behavior remains only as a compatibility baseline.

### Implement cross-request batching immediately

Rejected for the first slice. The fixed transaction cost and batch gain are unmeasured, and `synchronous=NORMAL` weakens the default fsync-amortization argument. Immediate implementation becomes justified only at the F3 thresholds.

### Tune `busy_timeout` or the five-second timeout upward

Rejected as a scaling decision. Longer waiting does not increase service capacity and can turn overload into hidden latency and retry ambiguity. Stage-specific deadlines may change after measurement, but timeout tuning is not an ownership or throughput architecture.

### Acknowledge writes on enqueue

Rejected. It converts accepted work into potentially lost or outcome-unknown work without a durable handle and idempotency contract. The chosen acknowledgement point remains after commit.

### Strict global FIFO

Rejected as the default because long bulk work can consume the complete latency budget of interactive writes, and no current evidence establishes a product requirement for global arrival-order completion. FIFO remains within explicit semantic ordering keys. A demonstrated consumer requirement flips this alternative.

### Put audit in the semantic transaction

Rejected while audit remains best-effort. Doing so would make audit failure roll back successful domain work and would increase the semantic transaction's residence time. If compliance requires atomic audit, that is a product-semantic change requiring an explicit fail-closed decision.

### Treat namespaces as shards

Rejected. ADR-007 defines namespace as attribution rather than a physical boundary, and one namespace column in one SQLite file still shares one WAL writer.

### Experimental SQLite multiwriter branches

Rejected for production qualification in this ADR. They add custom-build, compatibility, conflict, recovery, and operational risks and do not erase serialized commit or hot-page conflicts. They may be researched separately after the bundled-version correctness gate is resolved.

### PostgreSQL backend migration

Out of scope, not rejected on technical merit. It changes the hot-dataset concurrency model but is already tracked separately in the multi-backend roadmap and carries storage, migration, pooling, operations, FTS, and vector-parity work beyond this decision.

### Per-tenant physical sharding

Out of scope, not rejected on technical merit. Independent files can scale aggregate capacity, but tenancy distribution, hot-tenant behavior, routing epochs, cross-tenant operations, migrations, backups, and active-file budgets are product dependencies this ADR cannot decide. A logical namespace is not a substitute.

## Provenance

This decision is grounded in a static write-path code census (every cited path and symbol re-read at a pinned revision), and in an independent queueing-theory analysis of embedded-store writer scaling whose `NEEDS-EXPERIMENT` qualifications bound how its claims are used here. The bundled SQLite version was established from the vendored `libsqlite3-sys` source, not from a claim. A bundled-SQLite upgrade past the WAL-reset fix serves the F7 version gate and remains an open prerequisite in this tree (`libsqlite3-sys` 0.31.0 / SQLite 3.48.0 at the time of writing); it should merge before any concurrency experiment produces an architectural verdict.

### 2026-08-09 amendment: autocheckpoint is no longer an application-path experiment

Production checkpoint ownership now disables `wal_autocheckpoint` on every writer-capable
connection, as specified by ADR-091 Amendment 7. This supersedes the Context and Consequences
statements that describe `wal_autocheckpoint=4000` as current behavior, and removes F7's
current-versus-disabled comparison from the production topology matrix. An isolated benchmark may
still issue a raw pragma to construct a historical control, but neither `PoolConfig` nor an
environment variable can re-enable implicit checkpoint I/O in a shipped connection constructor.
