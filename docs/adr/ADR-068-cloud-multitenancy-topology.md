# ADR-068: Cloud Multi-Tenancy Topology and Tenant Isolation

**Status**: Proposed\
**Date**: 2026-06-23\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-007 (Namespace as Attribution), ADR-018 (Authorization Gate), ADR-049
(khived daemon), ADR-057 (Comm Actor-Addressed Delivery)\
**Amends (future condition)**: ADR-018 — see §"Future seam: multi-tenant single-process"\
**Related issues**: #199 (comm.inbox actor-filter bypass), #200 (from_actor mis-stamp), #13
(gate actor-identity gap), ADR-053 (ActorStore/SessionStore, cloud-tier actor threading)

---

## Context

### Cloud requirement

khive-cloud (Fly.io) serves multiple tenants over a shared infrastructure. Each tenant is an
independent agent deployment with its own KG corpus, inbox, memory, and task graph. The
system must guarantee that Tenant A cannot read Tenant B's data, messages, or memories under
any operational condition, including misconfiguration.

### The authorization seam today

ADR-007 establishes that namespace is attribution-only: a write-stamp on records, queryable
and filterable, but not a storage boundary. ADR-018 designates the Gate trait as the single
enforcement seam. The Gate receives a `GateRequest` carrying `actor`, `namespace`, `verb`, and
`args` at each dispatch call, and returns `Allow`, `Deny`, or fails open on infrastructure
error.

This architecture is correct for the OSS local deployment. For cloud, a `TenantGate`
implementation plugged into the same seam would be the natural isolation mechanism.

### The gap: dispatch hardcodes anonymous actor identity

The isolation audit (scope-cluster2-isolation.md, 2026-06-23) found a structural gap: the
gate always receives `ActorRef::anonymous()`, regardless of which tenant is making the
request.

In `crates/khive-runtime/src/pack.rs` at line 852:

```rust
let gate_req = GateRequest::new(ActorRef::anonymous(), ns, verb, params.clone());
```

Actor identity is minted separately, at lines 940-942, from static per-process config:

```rust
let configured_actor = match self.actor_id.as_deref() {
    Some(id) if !id.trim().is_empty() => ActorRef::new("actor", id),
    _ => ActorRef::anonymous(),  // id = "local"
};
```

`actor_id` is populated from `[actor] id` in `khive.toml` or the `KHIVE_ACTOR` environment
variable. It is a static per-process value, not a per-request authentication decision. As a
result:

- A `TenantGate` keying on `GateRequest.actor` always sees `anonymous` and cannot distinguish
  tenants.
- `comm.inbox` at `crates/khive-pack-comm/src/handlers.rs` line 131 reads
  `token.actor().id` to filter messages. When that id is `"local"` (the fallback), line 160's
  guard (`if caller_actor != "local"`) skips the filter and returns all inbound messages in
  the namespace — a party-line inbox (issue #199).
- `comm.send` stamps `from_actor` from the same `token.actor().id` at handlers.rs line 73,
  producing `"local"` attribution on every outbound message when actor is not configured
  (issue #200).

The startup code at `crates/khive-mcp/src/serve.rs` lines 190-199 emits a warning when
`actor_id` is unconfigured and the `comm` pack is loaded. The warning fires correctly but
produces no enforcement. The predicate is `should_warn_unattributed` at serve.rs line 268.

### Storage model

The storage brief (scope-conditional-storage.md, 2026-06-23) established:

- Postgres: 100% greenfield. `grep -r "postgres|postgresql|tokio_postgres" crates/` returns no
  storage-path code. `crates/khive-db/src/migrations.rs` has a reserved `postgres` field in
  `ServiceSchemaPlan` (line 37) that `apply_schema_plan` never iterates. ADR-009 explicitly
  defers Postgres to a future `khive-db-postgres` crate with its own backend contract tests.
- Storage traits in `crates/khive-storage/src/` (8 files: sql.rs, note.rs, entity.rs,
  graph.rs, event.rs, vectors.rs, sparse.rs, text.rs) contain zero SQLite-specific imports.
  `StorageError::Driver` wraps `Box<dyn StdError>` to accommodate any backend error type.
  The abstraction is backend-agnostic at the trait layer.
- The runtime layer is not yet backend-agnostic. `crates/khive-runtime/src/runtime.rs` imports
  `use khive_db::StorageBackend` directly at line 8 and holds `Arc<StorageBackend>` (the
  concrete SQLite type) at line 32. `from_backend` at line 131 takes `Arc<StorageBackend>`.
  ADR-028 describes a multi-backend boot sequence (`[[backends]]` TOML) that would abstract
  this seam; it is not yet wired.
- SQLite write concurrency is per-file: one `Arc<Mutex<Connection>>` protects the writer
  connection per `ConnectionPool` instance (`crates/khive-db/src/pool.rs` lines 52-60). With
  separate files per tenant, write mutexes are independent: 1,000 simultaneous tenant writes
  use 1,000 independent serialization points. The WAL-wedge failure class (stale process holds
  SQLite WAL, returns -32000 on reconnect) is per-file and therefore per-tenant.

---

## Decision

### One process per tenant, one SQLite file per tenant

khive-cloud multi-tenancy is implemented as **physical isolation**: each tenant runs in a
dedicated `khive-mcp` process opening a dedicated SQLite file. No tenant process opens any
other tenant's database file. No two tenants share a process.

**Operator contract**: every tenant process must be launched with a mandatory, non-empty
`KHIVE_ACTOR` environment variable (or equivalent `--actor` CLI flag) set to the tenant's
canonical identity string (for example, `tenant:acme-corp`). The `should_warn_unattributed`
predicate in serve.rs line 268 already detects the unconfigured-actor condition. A companion
PR promotes this from a warning to a startup error when the `comm` pack is loaded and
`actor_id` is absent or resolves to `"local"`, implementing strict mode. That PR is a
prerequisite for cloud deployment; this ADR records the topology it enforces.

**Isolation mechanism**: because Tenant A and Tenant B are separate OS processes opening
separate files, the isolation is physical, not policy-enforced. Tenant A's process has no file
descriptor to Tenant B's SQLite file. There is no gate policy to misconfigure, no RLS rule to
miswrite, and no namespace check to bypass. The blast radius of any single-tenant failure
(corruption, WAL wedge, disk exhaustion, process crash) is bounded to that tenant's process
and file.

**Actor identity flow under this topology**:

```
[KHIVE_ACTOR=tenant:acme] khive-mcp process
      |
      v
RuntimeConfig.actor_id = Some("tenant:acme")          (serve.rs config resolution)
      |
      v
VerbRegistry::dispatch (pack.rs:940-942)
      |
      +-- configured_actor = ActorRef::new("actor", "tenant:acme")
      |
      +-- token carries actor_id = "tenant:acme"          (NamespaceToken::mint_with_visibility)
      |
      +-- comm.inbox handler.rs:131 caller_actor = "tenant:acme"
      |   guard at line 160: "tenant:acme" != "local" -> filter IS applied
      |
      +-- comm.send handler.rs:73 from_actor = "tenant:acme"   (correctly attributed)
```

Note that `GateRequest` at pack.rs line 852 continues to carry `ActorRef::anonymous()` under
this topology. The gate does not receive the tenant's actor identity. This is intentional for
v1: the process boundary already provides physical isolation, so the gate's actor field is
irrelevant to tenant separation. `AllowAllGate` remains the OSS default and is appropriate for
a per-tenant process where there is only one tenant.

---

## How this ADR resolves issues #199, #200, and #13

### Issue #199 (comm.inbox actor-filter bypass) — structurally prevented

Under one-process-per-tenant, `KHIVE_ACTOR` is a mandatory operator contract enforced by the
strict-mode PR. When `actor_id = "tenant:acme"`, `token.actor().id = "tenant:acme"`, and the
`caller_actor != "local"` guard at handlers.rs line 160 is always true. The actor filter is
always applied. A tenant's inbox is scoped to messages addressed to their actor identity.

The cross-tenant scenario described in the audit requires a shared-process deployment where
`actor_id` is unconfigured. That deployment shape is not a valid khive-cloud topology under
this ADR. The strict-mode PR enforces this at startup.

### Issue #200 (from_actor mis-stamp) — structurally prevented as a corollary

`from_actor` at handlers.rs line 73 reads `token.actor().id`, which equals the configured
`KHIVE_ACTOR` value. With a correctly configured per-tenant process, all messages are
attributed to the tenant's actor identity. The mis-stamp occurs only in the unconfigured-actor
case, which strict mode prevents.

#200 requires no code fix beyond #199's strict-mode enforcement.

### Issue #13 (gate actor-identity gap) — not required for v1; future seam specified

Under per-process isolation, a `TenantGate` is not needed for v1 isolation. The gate at
pack.rs line 852 continues to receive `ActorRef::anonymous()` because no per-request auth
context exists. This is acceptable: the process boundary is the isolation mechanism, and no
gate policy needs to distinguish tenants within the same process, because there is only one
tenant per process.

Issue #13 becomes relevant only if a future deployment model requires multiple tenants to
share a single process (for cost efficiency at scale, or under a multi-tenant HTTP transport).
That path requires amending ADR-018 as described in the next section.

---

## Future seam: multi-tenant single-process (conditional, out of v1 scope)

If khive-cloud ever requires multiple tenants in one process, the following extension is
required before that deployment shape is valid:

1. An HTTP transport layer authenticates the incoming request (JWT, API key lookup) and
   extracts the tenant's canonical identity.
2. `VerbRegistry::dispatch` is extended to accept an optional `ActorRef` from the transport
   layer, passed alongside the DSL string.
3. `GateRequest::new` at pack.rs line 852 receives the authenticated `ActorRef` instead of
   `ActorRef::anonymous()`.
4. A `TenantGate` implementation uses `GateRequest.actor` to enforce per-tenant namespace and
   inbox isolation.
5. ADR-018 is amended to specify how authenticated caller identity reaches the gate, and to
   define the transport-to-dispatch contract.
6. ADR-007 Rule 2 (by-ID ops are namespace-agnostic) is reviewed in the cloud context:
   whether by-ID reads must be gated against the authenticated tenant's namespace is a cloud
   policy decision that the current OSS contract does not specify.

This extension is non-trivial and is out of scope for v1. ADR-053 (ActorStore, SessionStore)
addresses the longer-range actor threading design.

---

## Alternatives considered

### Shared-Postgres with row-level security

One Postgres instance serves all tenants. Tenant isolation is enforced by RLS policies keyed
on the tenant's authenticated identity. The Gate enforces at the verb level; RLS enforces at
the row level.

**Rejected for v1** because:

- Postgres is 100% greenfield in this codebase. The `khive-db-postgres` crate does not exist.
  `KhiveRuntime` at runtime.rs lines 8 and 32 holds `Arc<StorageBackend>`, the concrete
  SQLite type. Abstracting this to a trait object, writing the Postgres backend crate
  implementing all eight storage traits (`NoteStore`, `EntityStore`, `GraphStore`,
  `EventStore`, `VectorStore`, `SparseStore`, `TextSearch`, `SqlAccess`), authoring Postgres
  DDL migrations, and wiring RLS policies is full greenfield scope at every layer.
- Logical isolation is weaker than physical. A misconfigured RLS policy is a data breach. A
  process boundary cannot be misconfigured.
- The ADR-049 daemon model manages warm Vamana ANN indexes in-process. Postgres with pgvector
  eliminates in-process ANN state, removing cold-start concerns, but at the cost of lower peak
  recall (pgvector HNSW is adequate for cloud corpus sizes but does not match Vamana's 0.952
  recall at 1M vectors on production workloads).

Postgres is the correct path when the RAM ceiling imposed by N warm Vamana indexes becomes a
measured operational constraint at actual tenant scale. The storage traits are backend-agnostic,
`ServiceSchemaPlan.postgres` is reserved in migrations.rs, and the backend contract test
harness in `crates/khive-db/tests/contract/backend.rs` is explicitly designed to become a
cross-backend conformance suite. The path is open; the trigger is a measurement, not a
calendar date.

### Shared-process with gate-based auth-context threading

Multiple tenants share a single process. Authenticated caller identity is threaded from the
HTTP transport layer into `VerbRegistry::dispatch` and then into `GateRequest`. A `TenantGate`
enforces namespace and inbox isolation per request.

**Rejected for v1** because:

- This requires all the ADR-018 amendments described in the future-seam section above: new
  transport-level auth middleware, `dispatch` API change, `TenantGate` implementation, and
  ADR-007 Rule 2 review.
- Physical isolation from per-process deployment is a stronger guarantee and costs no
  additional code at v1. Rebuilding the auth threading before it is needed by operational
  scale is premature.
- The blast radius is larger: a compromised or crashed shared process affects all tenants.

### Namespace-as-tenant (explicit namespace= on every verb call)

Require cloud callers to supply `namespace=<tenant-id>` on every verb call. The gate enforces
that the authenticated tenant's identity matches the supplied namespace. This leverages the
existing fact that namespace does reach the gate (via `GateRequest.namespace`), unlike actor.

**Rejected** because it requires callers to carry an extra parameter on every request with
no mechanism to verify it was supplied correctly without a gate implementation that itself
requires auth-context threading. It trades one unsolved problem (actor not reaching the gate)
for another (namespace not authenticated by the transport) without reducing implementation
scope.

---

## Consequences

### Operational model

- Each tenant is an independent `khive-mcp` process launched with `KHIVE_ACTOR=<tenant-id>`
  and pointed at a dedicated SQLite file path (for example,
  `/data/tenants/<tenant-id>/khive.db`).
- Tenant provisioning: create the tenant directory, set `KHIVE_ACTOR`, launch the process.
  `StorageBackend::sqlite(path)` creates the file on first open; `run_migrations` applies the
  schema.
- Process count scales linearly with active tenants. At khive-cloud's initial deployment
  scale (tens to low hundreds of tenants), this is operationally straightforward and maps
  cleanly to Fly.io Machine-per-tenant deployment.

### Memory and ANN warm-state

Each tenant process carries one warm Vamana index (ADR-049 daemon model). RAM consumption
scales linearly with the number of simultaneously warm processes. For tenants with large
knowledge corpora, the index restore cost (ADR-049 §"cold start") applies once at process
launch, not per-request. Idle tenants with no active process incur no RAM cost.

At the scale where N simultaneously warm indexes exceeds available fleet RAM, the Postgres
escape hatch applies: build `khive-db-postgres`, eliminate in-process ANN state, consolidate
to fewer processes. That trigger should be measured from production metrics, not estimated.

### WAL and write concurrency

Per-tenant SQLite files give each tenant an independent write mutex and independent WAL
checkpoint thread. A WAL-wedge in one tenant's process does not affect any other tenant.
Cross-tenant write parallelism is free because writes go to independent files.

### Migration path to Postgres

When the Postgres trigger fires:

1. Build `crates/khive-db-postgres` implementing all eight storage traits.
2. Abstract `KhiveRuntime` away from `Arc<StorageBackend>` to `Arc<dyn SomeBackendHandle>`
   (the ADR-028 multi-backend abstraction already describes this target shape).
3. Write Postgres DDL migrations in `sql/` following the existing migration system.
4. Wire the ADR-018 amendments for auth-context threading.
5. Deploy a `TenantGate` enforcing per-tenant namespace isolation.
6. Migrate per-tenant SQLite data to the shared Postgres instance via export/import tooling.

Steps 1-3 are purely additive. Step 4 extends the dispatch API. Step 5 is a gate plugin.
Step 6 is a one-time migration operation. No tenant data is lost: per-tenant SQLite files
remain readable as an archival export format.

### Non-automatable floor

This topology requires human operator involvement to:

- Provision a new `KHIVE_ACTOR`-configured process per tenant at signup.
- Ensure no two tenant processes share a `KHIVE_ACTOR` value.
- Monitor per-process health; restart individual tenant processes on crash.

These are standard SaaS provisioning tasks and do not require ADR changes. Automation tooling
(tenant lifecycle scripts, Fly.io Machine management) is an operational concern outside the
scope of this ADR.

---

## Consequences summary

| Aspect                  | v1 (this ADR)                         | Future (Postgres path)                          |
| ----------------------- | ------------------------------------- | ----------------------------------------------- |
| Isolation               | Physical: separate process + file     | Logical: RLS + TenantGate                       |
| Actor threading to gate | Not required                          | Required (ADR-018 amendment)                    |
| #199 resolution         | Structurally prevented                | TenantGate enforces inbox scope                 |
| #200 resolution         | Structurally prevented (corollary)    | Correct attribution from auth layer             |
| #13 status              | Not required for v1                   | Requires full ADR-018 amendment                 |
| RAM cost                | Linear with warm tenants              | Constant (pgvector, no in-process ANN)          |
| Implementation cost     | Low: operator config + strict-mode PR | High: new crate, runtime abstraction, DDL       |
| Postgres trigger        | N/A                                   | Measured RAM ceiling from N warm Vamana indexes |
