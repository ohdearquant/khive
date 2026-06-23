# ADR-068: Cloud Multi-Tenancy Topology and Tenant Isolation

**Status**: Proposed\
**Date**: 2026-06-23\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-007 (Namespace as Attribution), ADR-018 (Authorization Gate), ADR-049
(khived daemon), ADR-057 (Comm Actor-Addressed Delivery)\
**Amends (effective now)**: ADR-007 Rule 4 (Cloud TenantGate clause); ADR-018
§"AllowAllGate" and §"Multi-tenant deployments" context bullet — see
§"Amendment to ADR-007 and ADR-018 for per-tenant-process topology" below\
**Related issues**: #199 (comm.inbox actor-filter bypass), #200 (from_actor mis-stamp), #13
(gate actor-identity gap), ADR-053 (ActorStore/SessionStore, cloud-tier actor threading)

---

## Context

### Cloud requirement

khive-cloud (Fly.io) serves multiple tenants over a shared infrastructure. Each tenant is an
independent agent deployment with its own KG corpus, inbox, memory, and task graph. The system
must guarantee that Tenant A cannot read Tenant B's data, messages, or memories.

### The authorization seam today

ADR-007 establishes that namespace is attribution-only: a write-stamp on records, queryable and
filterable, but not a storage boundary. ADR-018 designates the Gate trait as the single
enforcement seam. The Gate receives a `GateRequest` carrying `actor`, `namespace`, `verb`, and
`args` at each dispatch call, and returns `Allow`, `Deny`, or fails open on infrastructure
error.

This architecture is correct for the OSS local deployment. For cloud, a `TenantGate`
implementation plugged into the same seam would be the natural isolation mechanism in a
shared-process model.

### The gap: dispatch hardcodes anonymous actor identity

The isolation audit (scope-cluster2-isolation.md, 2026-06-23) found a structural gap: the gate
always receives `ActorRef::anonymous()`, regardless of which tenant is making the request.

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
variable (args.rs line 29). It is a static per-process value, not a per-request authentication
decision. As a result:

- A `TenantGate` keying on `GateRequest.actor` always sees `anonymous` and cannot distinguish
  tenants.
- `comm.inbox` at `crates/khive-pack-comm/src/handlers.rs` line 131 reads `token.actor().id`
  to filter messages. When that id is `"local"` (the fallback), line 160's guard
  (`if caller_actor != "local"`) skips the filter and returns all inbound messages in the
  namespace — a party-line inbox (issue #199).
- `comm.send` stamps `from_actor` from the same `token.actor().id` at handlers.rs line 73,
  producing `"local"` attribution on every outbound message when actor is not configured
  (issue #200).

The current startup code in serve.rs at line 190 emits a `tracing::warn!` when `actor_id` is
unconfigured and the `comm` pack is loaded (predicate: `should_warn_unattributed` at serve.rs
line 268). This warning is advisory only: it fires but produces no startup enforcement. A cloud
process booted without `KHIVE_ACTOR` and without a strict-mode enforcement mechanism still hits
the party-line and `"local"` mis-stamp paths.

### Storage model

The storage brief (scope-conditional-storage.md, 2026-06-23) established:

- Postgres: 100% greenfield. `grep -r "postgres|postgresql|tokio_postgres" crates/` returns no
  storage-path code. `crates/khive-db/src/migrations.rs` has a reserved `postgres` field in
  `ServiceSchemaPlan` (line 37) that `apply_schema_plan` never iterates (line 46 iterates only
  `plan.sqlite`). ADR-009 explicitly defers Postgres to a future `khive-db-postgres` crate with
  its own backend contract tests.
- Storage traits in `crates/khive-storage/src/` (8 files: sql.rs, note.rs, entity.rs, graph.rs,
  event.rs, vectors.rs, sparse.rs, text.rs) contain zero SQLite-specific imports.
  `StorageError::Driver` wraps `Box<dyn StdError>` to accommodate any backend error type. The
  abstraction is backend-agnostic at the trait layer.
- The runtime layer is not yet backend-agnostic. `crates/khive-runtime/src/runtime.rs` imports
  `use khive_db::StorageBackend` directly at line 8 and holds `Arc<StorageBackend>` (the
  concrete SQLite type) at line 32. `from_backend` at line 131 takes `Arc<StorageBackend>`.
  ADR-028 describes a multi-backend boot sequence (`[[backends]]` TOML) that would abstract
  this seam; it is not yet wired.
- SQLite write concurrency is per-file: one `Arc<Mutex<Connection>>` protects the writer
  connection per `ConnectionPool` instance (`crates/khive-db/src/pool.rs` lines 52-60). With
  separate files per tenant, write mutexes are independent: 1,000 simultaneous tenant writes use
  1,000 independent serialization points. The WAL-wedge failure class (stale process holds
  SQLite WAL, returns -32000 on reconnect) is per-file and therefore per-tenant.

---

## Decision

### One process per tenant, one SQLite file per tenant

khive-cloud multi-tenancy is implemented as **physical isolation**: each tenant runs in a
dedicated `khive-mcp` process opening a dedicated SQLite file. No tenant process opens any
other tenant's database file. No two tenants share a process.

### Cloud supervisor contract (normative)

The cloud supervisor (the process launcher, Fly.io machine manager, or equivalent) MUST enforce
the following invariants before a tenant process begins serving requests:

1. **Mandatory non-empty `KHIVE_ACTOR`**: every tenant process is launched with `KHIVE_ACTOR`
   set to the tenant's canonical identity string (for example, `tenant:acme-corp`). An empty or
   absent `KHIVE_ACTOR` is a provisioning error; the supervisor rejects the launch.
2. **Mandatory strict actor attribution**: every tenant process is launched with
   `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`. When this env var is set, a dedicated startup gate
   (`enforce_strict_actor_mode`, added by PR #220 in `crates/khive-mcp/src/serve.rs`) runs
   before the server begins serving requests. If the actor is absent or resolves to `"local"`
   and the `comm` pack is loaded, the gate returns an error and the process exits non-zero; it
   is never placed in service. This gate is a new parallel check — it does not modify the
   advisory `should_warn_unattributed` predicate (serve.rs line 268), which remains
   warning-only for non-strict deployments. The cloud launcher sets
   `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` unconditionally on every tenant process; it is not
   optional.

   **Dependency**: this enforcement is provided by PR #220 (branch
   `fix/comm-tenant-isolation-strict`, commit bd15e595, not yet merged to main at the time
   of this ADR). The cloud isolation guarantee for issues #199 and #200 holds once PR #220
   is merged and the cloud launcher sets both `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` and
   `KHIVE_ACTOR` per this supervisor contract. On plain `main` today (pre-#220) the env var
   has no effect. PR #220 must land before or concurrently with PR #218 (this ADR's PR).
3. **Unique actor/path pairs**: the supervisor derives the actor id and the SQLite file path
   atomically from the tenant registry. If a `(KHIVE_ACTOR, KHIVE_DB)` pair already exists in
   the registry for a live process, the new launch is rejected. Duplicate actor or duplicate
   path assignments are provisioning bugs; the supervisor does not paper over them.
4. **DB path from tenant registry only**: the SQLite file path (`KHIVE_DB`, args.rs line 15) is
   derived from the supervisor's tenant registry, not accepted from untrusted caller input.
   Tenant DB paths are confined to a dedicated directory (for example, `/data/tenants/`);
   paths outside that directory are rejected before the process is spawned.
5. **No shared-process deployment**: this topology does not support multiple tenants sharing one
   khive-mcp process. Any deployment that relaxes items 1-4 is outside this ADR's contract and
   requires the multi-tenant single-process extension described in the future-seam section.

**Isolation mechanism**: because Tenant A and Tenant B are separate OS processes opening
separate files, data-at-rest isolation is physical, not policy-enforced. Tenant A's process has
no file descriptor to Tenant B's SQLite file. There is no gate policy to misconfigure, no RLS
rule to miswrite, and no namespace check to bypass. The blast radius of any single-tenant
failure (corruption, WAL wedge, disk exhaustion, process crash) is bounded to that tenant's
process and file.

**Actor attribution correctness**: process/file isolation is the structural guarantee for
data-at-rest. Actor-attribution correctness — ensuring that `comm.inbox` is scoped to the
tenant's identity and that `comm.send` stamps the tenant's identity as `from_actor` — is
guaranteed by mandatory strict-mode startup, which the cloud supervisor sets unconditionally
via `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`. This is a deployment-layer enforcement: it holds
because the supervisor always sets it, not because any code path structurally prevents a
misconfigured boot. The two guarantees are complementary: physical isolation prevents
cross-tenant data access regardless of actor configuration; strict-mode startup prevents the
party-line inbox and mis-stamp by refusing to serve a misconfigured process.

**Actor identity flow under this topology**:

```
[KHIVE_ACTOR=tenant:acme KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1] khive-mcp process
      |
      v
KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1: enforce_strict_actor_mode() returns Err
      (startup fails) if actor is absent or "local" and comm pack is loaded
      [provided by PR #220; no-op on pre-#220 main]
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
      |   guard at line 160: "tenant:acme" != "local" -> to_actor filter IS applied
      |
      +-- comm.send handler.rs:73 from_actor = "tenant:acme"   (correctly attributed)
```

Note that `GateRequest` at pack.rs line 852 continues to carry `ActorRef::anonymous()` under
this topology. The gate does not receive the tenant's actor identity. This is intentional for
v1: the process boundary already provides physical isolation, so the gate's actor field is
irrelevant to tenant separation. `AllowAllGate` remains the OSS default and is appropriate for
a per-tenant process where there is only one tenant per process.

---

## Amendment to ADR-007 and ADR-018 for per-tenant-process topology

This section amends ADR-007 Rev 6 and ADR-018 effective with this ADR's acceptance. These
amendments apply to the per-tenant-process cloud topology described in this ADR only; the OSS
local-deployment contract of both parent ADRs is unchanged.

### Amendment to ADR-007 Rule 4 (Gate as single enforcement seam)

ADR-007 Rule 4 states: "Cloud: a TenantGate (non-OSS, separate crate behind the Gate trait)
validates the caller's authenticated identity and enforces per-tenant namespace access."

**Amendment**: for the per-tenant-process topology specified in this ADR, physical process and
file isolation replaces TenantGate as the tenant isolation mechanism. Each process serves
exactly one tenant, so there is no intra-process tenant to distinguish via gate policy. The
gate continues to receive `ActorRef::anonymous()` (pack.rs line 852) because no per-request
authentication context is threaded at v1, and no gate policy need key on tenant identity when
only one tenant's data is reachable from the process.

`AllowAllGate` is appropriate for this topology. ADR-018's note that "AllowAllGate is a
footgun in multi-user or hosted contexts" and "hosted deployments configure RegoGate or a
custom backend before serving traffic" (ADR-018 §"AllowAllGate: the OSS default") does not
apply to per-tenant-process deployments: each process is single-tenant, and `AllowAllGate`
within a process that serves only one tenant is not a multi-user footgun. The multi-user
concern arises only when one process serves multiple tenants, which this topology prohibits
(cloud supervisor contract item 5 above).

TenantGate actor-to-gate threading (issue #13) is required only if a future deployment model
places multiple tenants in a single process (the multi-tenant single-process extension in the
future-seam section). For v1 per-tenant-process deployments, #13 is not required.

### Amendment to ADR-018 §"Multi-tenant deployments" context bullet

ADR-018 Context §"Multi-tenant deployments" defines multi-tenant deployments as "one khive
process serving multiple users or agents" and identifies this as a scenario where the absence
of gating is "a deal-breaker." ADR-007 Rule 4 (verified: ADR-007-namespace.md line 349) is
where a `TenantGate` (non-OSS, separate crate behind the Gate trait) is named as the cloud
isolation mechanism for that shared-process model. This ADR establishes an alternative:
per-process physical isolation defers the need for TenantGate to shared-process scale.

For v1 cloud deployments under this ADR: the authorization seam (ADR-018 Gate trait) is live
at every verb dispatch, `AllowAllGate` is installed, and audit events fire through tracing per
the ADR-018 contract. The change is that the "configure a policy backend before serving
traffic" recommendation for multi-tenant deployments is satisfied by the process boundary
rather than by a TenantGate implementation. When the per-tenant-process topology is in use,
AllowAllGate is not a misconfiguration.

ADR-018 §"Hard enforcement" and §"Fail-open on gate Err" are unchanged and apply verbatim.

---

## How this ADR resolves issues #199, #200, and #13

### Issue #199 (comm.inbox actor-filter bypass) — enforced by mandatory strict-mode startup

Under one-process-per-tenant, `KHIVE_ACTOR` is a mandatory operator contract and
`KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` is a mandatory cloud supervisor setting. When
`actor_id = "tenant:acme"`, `token.actor().id = "tenant:acme"`, and the
`caller_actor != "local"` guard at handlers.rs line 160 is always true: the actor filter is
always applied.

One precision on the filter: handlers.rs lines 160-164 apply `FilterOp::EqOrMissing` for the
`$.to_actor` JSON path. This operator matches rows where `to_actor` equals the caller's label
OR where the field is absent or NULL (see `crates/khive-storage/src/note.rs` line 233 for the
`EqOrMissing` variant definition). Legacy messages stored without a `to_actor` property are
therefore visible to an actor-scoped inbox query on a database that contains them.

For v1 cloud, this legacy path is a no-op on fresh per-tenant databases: a freshly provisioned
tenant DB has no pre-existing messages and therefore no legacy rows without `to_actor`. The
cloud supervisor provisions a new empty SQLite file per tenant (via
`StorageBackend::sqlite(path)` on first open), so no legacy rows are present at provisioning
time.

If tenant data is migrated or imported from an existing database, a strict equality filter plus
a quarantine pass is required: any imported messages without `to_actor` must be either assigned
the tenant's actor label or quarantined before the tenant process is placed in service. The
cloud migration runbook must include this step.

The cross-tenant scenario described in the audit requires a shared-process deployment where
`actor_id` is unconfigured. That deployment shape is not a valid khive-cloud topology under
this ADR.

#### Amendment: EqOrMissing is the current implemented behavior — corrects ADR-057

ADR-057 §"`comm.inbox` behavior change" (ADR-057-comm-actor-addressed-delivery.md lines
~201–204) states that actor-scoped callers do not see messages with a missing `to_actor`
field. The implemented code does not match this description. The actual filter applied when
`caller_actor != "local"` uses `FilterOp::EqOrMissing` (verified: `crates/khive-pack-comm/src/handlers.rs`
lines 160–165; `crates/khive-storage/src/note.rs` lines 232–234): this operator matches rows
where `$.to_actor` equals the caller's label **OR** where the field is absent or NULL. Legacy
messages without a `to_actor` property are therefore visible to actor-scoped inbox reads.

**This ADR's amendment is authoritative**: the current implemented behavior is
`EqOrMissing` (missing `to_actor` IS visible). ADR-057's strict-equality description is
incorrect as implemented. The cloud consequence is that a tenant DB that contains legacy
messages without `to_actor` will expose those messages to the tenant's actor-scoped inbox
queries. For fresh cloud deployments this is a no-op (no legacy rows). For migrated or
imported databases the quarantine rule already stated above — assign or quarantine all messages
lacking `to_actor` before placing the process in service — is mandatory and sufficient to close
this gap.

ADR-057 itself is not rewritten in this PR (PR #218 is scoped to ADR-068). ADR-057's text
requires a follow-up correction to align its description with the implemented
`EqOrMissing` behavior; that correction is tracked as a follow-up to this ADR.

### Issue #200 (from_actor mis-stamp) — enforced by mandatory strict-mode startup

`from_actor` at handlers.rs line 73 reads `token.actor().id`, which equals the configured
`KHIVE_ACTOR` value. With mandatory strict-mode startup preventing an unconfigured-actor
process from serving, all messages are attributed to the tenant's actor identity. The mis-stamp
occurs only in the unconfigured-actor case, which the supervisor contract eliminates.

Issue #200 requires no code fix beyond strict-mode enforcement at startup.

### Issue #13 (gate actor-identity gap) — not required for v1; future seam specified

Under per-process isolation, a `TenantGate` is not needed for v1 isolation. The gate at
pack.rs line 852 continues to receive `ActorRef::anonymous()` because no per-request auth
context exists. This is acceptable: the process boundary is the isolation mechanism, and no
gate policy needs to distinguish tenants within the same process because there is only one
tenant per process.

Issue #13 becomes relevant only if a future deployment model requires multiple tenants to share
a single process. That path requires the amendments described in the future-seam section.

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
  `KhiveRuntime` at runtime.rs lines 8 and 32 holds `Arc<StorageBackend>`, the concrete SQLite
  type. Abstracting this to a trait object requires cross-crate API migration: runtime.rs's
  `from_backend` constructor (line 131) and all callers must accept a trait-object backend
  handle. Writing the Postgres backend crate implementing all eight storage traits
  (`NoteStore`, `EntityStore`, `GraphStore`, `EventStore`, `VectorStore`, `SparseStore`,
  `TextSearch`, `SqlAccess`), authoring Postgres DDL migrations, and wiring RLS policies is
  full greenfield scope at every layer.
- Logical isolation is weaker than physical. A misconfigured RLS policy is a data breach. The
  process/file boundary removes RLS-policy drift from the isolation path; wrong actor/path
  provisioning remains a supervisor risk, mitigated by the invariants in the cloud supervisor
  contract above (mandatory non-empty `KHIVE_ACTOR`, unique actor/path pairs, path from tenant
  registry only).
- The ADR-049 daemon model manages warm Vamana ANN indexes in-process. Postgres with pgvector
  eliminates in-process ANN state, removing cold-start concerns, but at the cost of lower peak
  recall (pgvector HNSW is adequate for cloud corpus sizes but does not match Vamana's 0.952
  recall at 1M vectors on production workloads).

Postgres is the correct path when the RAM cost of N simultaneously warm Vamana indexes exceeds
the fleet RAM budget. The trigger is defined in the Consequences section below.

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

**Rejected** because it requires callers to carry an extra parameter on every request with no
mechanism to verify it was supplied correctly without a gate implementation that itself requires
auth-context threading. It trades one unsolved problem (actor not reaching the gate) for
another (namespace not authenticated by the transport) without reducing implementation scope.

---

## Consequences

### Operational model

- Each tenant is an independent `khive-mcp` process launched with `KHIVE_ACTOR=<tenant-id>`,
  `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`, and pointed at a dedicated SQLite file path (for example,
  `/data/tenants/<tenant-id>/khive.db`).
- Tenant provisioning: the supervisor creates the tenant directory, derives the actor id and DB
  path from the tenant registry, validates uniqueness, sets mandatory env vars, and launches the
  process. `StorageBackend::sqlite(path)` creates the file on first open; `run_migrations`
  applies the schema.
- Process count scales linearly with active tenants. At khive-cloud's initial deployment scale
  (tens to low hundreds of tenants), this is operationally straightforward and maps cleanly to
  Fly.io Machine-per-tenant deployment.

### Deployment checklist (normative, cloud supervisor must verify before process is placed in service)

1. `KHIVE_ACTOR` is set to a non-empty, non-`"local"` tenant identity string.
2. `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` is set in the process environment.
3. The `(KHIVE_ACTOR, KHIVE_DB)` pair is unique across all live processes in the tenant
   registry.
4. `KHIVE_DB` resolves to a path inside the designated tenant data directory.
5. If the tenant DB was migrated or imported: all messages with a missing `to_actor` property
   have been assigned the tenant's actor label or quarantined before process launch.
6. Process startup exit code is zero (strict-mode check passed).

### Memory and ANN warm-state

Each tenant process carries one warm Vamana index (ADR-049 daemon model). RAM consumption
scales linearly with the number of simultaneously warm processes. For tenants with large
knowledge corpora, the index restore cost (ADR-049 §"cold start") applies once at process
launch, not per-request. Idle tenants with no active process incur no RAM cost.

### Postgres trigger (normative)

The Postgres migration path activates when the following condition is met in production metrics:

```
active_warm_tenants * p95_rss_per_warm_vamana_index > fleet_ram_budget * 0.70
```

where:

- `active_warm_tenants`: count of tenant processes with a loaded Vamana index, measured from
  process-level RSS metrics in the fleet monitoring system (Fly.io metrics or equivalent).
- `p95_rss_per_warm_vamana_index`: the 95th-percentile per-process RSS increment attributable
  to a warm Vamana index, measured by comparing RSS of idle-process and index-loaded-process
  samples over a rolling 7-day window.
- `fleet_ram_budget`: total available fleet RAM as reported by the infrastructure provider.
- `0.70`: safety factor (30% headroom). Adjust only via an ADR amendment.

Decision owner: lambda:khive. Review artifact: a filed ADR amendment with the measured metric
values, a 7-day trend chart, and a proposed migration timeline. No Postgres migration may begin
without that artifact.

### WAL and write concurrency

Per-tenant SQLite files give each tenant an independent write mutex and independent WAL
checkpoint thread. A WAL-wedge in one tenant's process does not affect any other tenant.
Cross-tenant write parallelism is free because writes go to independent files.

### Migration path to Postgres

When the Postgres trigger fires:

1. Build `crates/khive-db-postgres` implementing all eight storage traits.
2. Migrate `KhiveRuntime` away from `Arc<StorageBackend>` to a trait-object backend handle
   (the ADR-028 multi-backend abstraction already describes this target shape). This is
   non-additive compatibility work: runtime.rs's `from_backend` constructor (line 131) and all
   callers in the codebase must be updated to accept the new handle type, and
   `cargo check --workspace` must pass after the change.
3. Write Postgres DDL migrations in `sql/` following the existing migration system.
4. Wire the ADR-018 amendments for auth-context threading.
5. Deploy a `TenantGate` enforcing per-tenant namespace isolation.
6. Migrate per-tenant SQLite data to the shared Postgres instance via export/import tooling.

Steps 1 and 3 are additive (new crate, new SQL files). Step 2 is a cross-crate API migration
that touches runtime.rs plus all callers. Steps 4 and 5 extend the dispatch API and introduce
a new gate implementation. Step 6 is a one-time migration operation. No tenant data is lost:
per-tenant SQLite files remain readable as an archival export format.

### Non-automatable floor

This topology requires cloud supervisor involvement to:

- Provision a new `KHIVE_ACTOR`-configured process per tenant at signup, validating uniqueness
  in the tenant registry.
- Ensure no two tenant processes share a `KHIVE_ACTOR` value or a DB path.
- Monitor per-process health; restart individual tenant processes on crash.

These are standard SaaS provisioning tasks and do not require ADR changes. Automation tooling
(tenant lifecycle scripts, Fly.io Machine management) is an operational concern outside the
scope of this ADR.

---

## Consequences summary

| Aspect                   | v1 (this ADR)                             | Future (Postgres path)                        |
| ------------------------ | ----------------------------------------- | --------------------------------------------- |
| Isolation                | Physical: separate process + file         | Logical: RLS + TenantGate                     |
| Actor threading to gate  | Not required                              | Required (ADR-018 amendment)                  |
| AllowAllGate suitability | Appropriate (one tenant per process)      | Footgun; TenantGate required                  |
| #199 resolution          | Enforced by mandatory strict-mode startup | TenantGate enforces inbox scope               |
| #199 legacy EqOrMissing  | No-op on fresh tenant DBs; migration step | Strict equality filter + quarantine pass      |
| #200 resolution          | Enforced by mandatory strict-mode startup | Correct attribution from auth layer           |
| #13 status               | Not required for v1                       | Requires full ADR-018 amendment               |
| RAM cost                 | Linear with warm tenants                  | Constant (pgvector, no in-process ANN)        |
| Implementation cost      | Low: supervisor config + strict-mode PR   | High: new crate, runtime API migration, DDL   |
| Postgres trigger         | N/A                                       | active_warm_tenants * p95_rss > budget * 0.70 |
