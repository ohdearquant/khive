# ADR-063: Comm Pack Principal Model and Remote Backend Isolation

**Status**: Proposed
**Date**: 2026-06-19
**Authors**: lambda:khive, alpha:architect
**Depends on**: ADR-007 Rev 7 (namespace carve-out for principal-scoped backends) |
ADR-028 (Pack-Scoped Backends) | ADR-040 (Communication and Schedule Packs) |
ADR-057 (Comm Actor-Addressed Delivery) | ADR-053 (ActorStore / SessionStore — pending)
**Related issues**: #75 (actor identity on every request) | #112 (khive-channel umbrella) |
#113 (Telegram adapter)
**Related ADRs**: ADR-056 (Channel Transport Layer — human out-of-band; sibling, not the same)

---

## Context

### The inbox leakage problem

The comm pack stores messages in the shared "local" SQLite namespace alongside all other pack
data. ADR-057 introduced actor-addressed delivery: `comm.send(to="lambda:leo")` stamps
`to_actor="lambda:leo"` on the inbound copy of the message, and `comm.inbox` filters by
`to_actor` when the caller's actor label is not "local".

The filter is wired and correct. However, it is dormant for the common case:

```rust
// handlers.rs, handle_inbox, line 131
let caller_actor = token.namespace().as_str().to_string();

// line 160
if caller_actor != "local" {
    // push to_actor filter
}
```

Every undecorated `kkernel mcp` session starts with namespace `"local"` because no `--actor`
flag and no `[actor] id` in `khive.toml` are present. `token.namespace().as_str()` therefore
returns `"local"` for every lambda, the `caller_actor != "local"` branch is never taken, and
`comm.inbox` returns all inbound messages in the namespace — the party-line leak that ADR-057
was designed to close.

The root cause is not the filter logic: the filter is correct. The root cause is that actor
identity is not yet carried as a distinct field on the `NamespaceToken`. The current code uses
the namespace string as a proxy for actor identity. When every actor uses the same namespace,
the proxy collapses and per-actor filtering cannot activate.

### The remote backend requirement

Ocean has identified cross-machine lambda-to-lambda communication as a roadmap item: lambdas
running in separate sandboxes or on separate machines need to exchange messages. This requires
a transport with a fundamentally different trust model.

In a shared local SQLite store, all actors with store access can query any row. Per-actor
scoping is enforced by a view-layer filter that any client could bypass. This is acceptable for
a single-machine, co-located, trusted deployment. It is not acceptable as the security model
for a multi-machine, mutually untrusting deployment.

A remote message broker enforces per-principal scope server-side: each lambda authenticates
with its own credential, the broker validates the credential on every connection, and read/write
operations are scoped to the authenticated principal by the server. No client can receive
another principal's messages without a valid credential for that principal. This is the correct
trust model for multi-machine delivery.

### Relationship to ADR-056 (Channel Transport Layer)

ADR-056 addresses human-facing out-of-band transport: delivering notifications to the
maintainer via Telegram, email, or WhatsApp, and ingesting replies into `comm.inbox`. The
maintainer is a human, not a lambda. ADR-056's design is one-directional from the
maintainer's perspective (they receive and reply via external channels) and does not address
lambda-to-lambda machine communication.

This ADR addresses lambda-to-lambda broker communication: each lambda is a principal that
sends and receives messages from other lambda principals. The scope is orthogonal. A
deployment may run both: ADR-056's channel adapter delivers human messages through the comm
verb surface; this ADR's broker delivers lambda-to-lambda messages through the same verb
surface. The pack interface (`comm.send`, `comm.inbox`, `comm.reply`, `comm.thread`,
`comm.read`) is unchanged in either case.

---

## Current State / Bug

The following lines document the current behavior that produces the inbox leak. They are quoted
from the shipped source as of 2026-06-19 and represent the state before issue #75 lands.

**`crates/khive-pack-comm/src/handlers.rs`, `handle_inbox`, lines 131 and 160:**

```rust
let caller_actor = token.namespace().as_str().to_string();
// ...
if caller_actor != "local" {
    property_filters.push(PropertyFilter {
        json_path: "$.to_actor".to_string(),
        op: FilterOp::EqOrMissing,
        value: SqlValue::Text(caller_actor.clone()),
    });
}
```

The actor label is derived from `token.namespace().as_str()`. In the default deployment,
every session's namespace is `"local"`, so `caller_actor == "local"` and the `to_actor`
filter is skipped. All inbound messages in the "local" namespace are returned to every caller.

**`crates/khive-pack-comm/src/handlers.rs`, `handle_send`, lines 72-74:**

```rust
let caller_ns = token.namespace().as_str().to_string();
let from_actor = caller_ns.clone();
let to_actor = p.to.trim().to_string();
```

`from_actor` is set to the namespace string. For every undecorated session, `from_actor` is
`"local"`. Messages sent by `lambda:khive` and `lambda:leo` within a default deployment are
both stamped `from_actor="local"`, making sender attribution ineffective.

**`crates/khive-runtime/src/config.rs`, `RuntimeConfig` struct:**

`RuntimeConfig` has no `actor_id` field. The token carries an `ActorRef` (line 77) but the
`mint_authorized` constructor takes an `ActorRef::anonymous()` everywhere the token is minted
in the current code. There is no per-lambda authenticated principal in the token at this time.

The fix for the leak — making the `to_actor` filter activate — requires the token to carry a
distinct, non-`"local"` actor label per lambda. That is issue #75.

---

## Decision

### 1. Principal model

For the purposes of the comm pack, a **principal** is the entity that sends and receives
messages. In the current OSS deployment, the principal is identified by its actor label. In a
remote broker deployment, the principal is identified by its authenticated credential.

The principal model has two implementations, selected by the backend the comm pack uses:

**1a. Local SQLite backend (current, degenerate):**

Principal identity is the actor label derived from the runtime context. The actor label is
currently `token.namespace().as_str()` — a proxy that collapses when multiple actors share the
same namespace. The per-actor inbox filter (`to_actor`) is forward-deployed but dormant until
issue #75 lands and the token carries a distinct actor identity per lambda.

This implementation is correctness-for-now. It is explicitly NOT a security boundary. Any
process with SQLite store access can read any message row regardless of the `to_actor`
property. The isolation is a view-layer courtesy for cooperating, co-located actors.

**1b. Remote broker backend (roadmap):**

Principal identity is an authenticated credential held by the lambda process. The credential
is established at connection time (out-of-band from the MCP verb surface). The broker enforces
per-principal scope server-side: every read and write is scoped to the authenticated principal
by the server, not by a client-side filter. No lambda can receive another lambda's messages
without a valid credential for that principal.

The authentication mechanism for the remote broker (credential format, rotation, revocation)
is left to a future transport ADR. This ADR specifies only that such a mechanism is required
and that it constitutes the security boundary, in contrast to the local view-layer filter.

### 2. Storage partitioning

**Local SQLite backend:** messages are stored in the shared notes table with the "local"
namespace, alongside all other pack data. No per-principal partition exists at the storage
layer. Isolation is entirely view-layer (`to_actor` property filter).

**Remote broker backend:** storage is partitioned per principal at the broker. The partition
boundary is defined and enforced by the broker server. The khive pack interface does not
specify the partitioning mechanism; it is internal to the broker. From the pack's perspective,
`comm.send(to="lambda:leo")` delivers to the `lambda:leo` principal's partition, and
`comm.inbox` reads from the authenticated caller's partition.

### 3. Relationship to ADR-007

This ADR invokes the ADR-007 Rev 7 Rule 8 carve-out: the comm pack may declare a backend
whose trust model is principal-scoped isolation. The carve-out explicitly does not affect
Rules 0-7 for the shared local substrate. KG, memory, gtd, brain, schedule, and knowledge
packs are unaffected by this ADR.

The broker backend's server-side principal scope is an instance of the Gate (ADR-007 Rule 4:
"authorization enforced at one seam: the Gate"). The broker authenticates the connection and
scopes all I/O to the authenticated principal, in the same way that a cloud TenantGate
validates a cloud tenant's identity before allowing verb dispatch. The seam is different (TCP
connection vs. MCP dispatch path) but the principle is the same: one enforcement point, at the
trust boundary.

### 4. Relationship to ADR-028

The comm pack's remote broker is a pack-scoped backend in the sense of ADR-028: it is a
distinct storage profile with different isolation properties than the shared SQLite store.
ADR-028's deferred multi-backend TOML configuration is the mechanism by which the comm pack
would declare a non-default backend in a future deployment. Until that machinery ships, the
comm pack uses the single shared SQLite backend (the degenerate case).

ADR-028 §1 notes that multi-backend configuration is deferred. This ADR does not depend on
ADR-028's multi-backend machinery landing first; the remote broker work is separately scoped.

### 5. Migration and sequencing

The work to close the inbox leak and enable remote delivery proceeds in three steps:

**Step 1 — Actor identity plumbing (issue #75, prerequisite for both Step 2 and Step 3):**

The `NamespaceToken` must carry a distinct, non-anonymous `ActorRef` per lambda. This requires
`RuntimeConfig` to gain an `actor_id` field (or equivalent) that is populated from the
`[actor] id` configuration and threaded through `VerbRegistry::dispatch` into the token at
mint time. Until this lands, the `caller_actor == "local"` condition in `handle_inbox` is
always true and the `to_actor` filter cannot activate.

Changes implied by Step 1:

- `RuntimeConfig`: add `actor_id: Option<String>` or equivalent.
- `VerbRegistry::dispatch` (or the token mint site): populate `ActorRef` from `actor_id`
  rather than always `ActorRef::anonymous()`.
- `comm` handlers: derive actor label from `token.actor().id` when it is non-anonymous,
  falling back to `token.namespace().as_str()` for undecorated sessions (backward
  compatibility).

Step 1 is shared infrastructure. It is not comm-specific. The comm pack is the primary
beneficiary for inbox filtering, but the actor identity is useful for attribution across all
packs (ADR-053).

**Step 2 — Local inbox scope (fix the leak):**

Once Step 1 is in place, the `comm.inbox` filter activates automatically: `caller_actor` is
no longer always `"local"`, and messages addressed to a specific actor are returned only to
that actor. No additional code change is required in the handlers beyond Step 1. The
`to_actor` filter, the `idx_comm_message_to_actor` index, and the `EqOrMissing` filter are
already forward-deployed in the current code.

This resolves the inbox leak for the single-machine deployment. The security model remains
the local view-layer filter (not a security boundary for adversarial principals, but adequate
for co-located, cooperating lambdas).

**Step 3 — Remote broker backend (cross-machine delivery):**

The remote broker backend is a future implementation. It requires:

- An authentication mechanism per lambda (credential format TBD in a transport ADR).
- A broker implementation that enforces per-principal scope server-side.
- Pack-scoped backend configuration (ADR-028 multi-backend machinery or an equivalent).
- The same `comm.*` verb surface, unchanged. Callers do not change; only the backend changes.

Step 3 is blocked on Step 1 (actor identity must exist before it can be used as an
authentication credential basis) and on the transport ADR (which specifies the credential
format and broker protocol). It is NOT blocked on Step 2; the local fix and remote broker
can be developed in parallel once Step 1 lands.

### 6. Pack verb surface

The `comm.*` verb surface is unchanged by this ADR. `comm.send`, `comm.inbox`, `comm.reply`,
`comm.thread`, and `comm.read` work identically against the local SQLite backend and the
remote broker backend. The backend is an implementation detail of the comm pack; callers
observe the same behavior.

---

## Consequences

### Positive

- The party-line inbox leak has a documented root cause (missing actor identity in the token)
  and a clear fix path (Step 1 above), rather than being an architectural ambiguity.
- The remote broker roadmap item is specified at the ADR level: the isolation contract, the
  sequencing, and the relationship to existing ADRs are all written down.
- ADR-007's "namespace = attribution, not isolation" claim is preserved for the shared
  substrate. The comm pack carve-out does not infect other packs.
- The verb surface is stable: no caller changes are required when the backend changes.

### Negative

- Two isolation models coexist: view-layer filter for the local backend; server-side principal
  scope for the remote broker. Contributors must read this ADR to understand which applies.
- The local model is not a security boundary. This must be documented clearly in operational
  documentation and communicated to any deployment that co-locates mutually untrusting lambdas.
- Step 1 (issue #75) is a prerequisite that spans multiple packs and the runtime dispatch path.
  Until it lands, the inbox leak persists.

---

## Alternatives Considered

### Option A: Local view-layer filter as permanent isolation model

Keep the shared SQLite store and the `to_actor` filter as the sole isolation mechanism.
Do not plan a remote broker backend.

Rejected for multi-machine deployments. A shared store filter is bypassable by any process
with store access. For co-located, cooperating lambdas this is acceptable; for
cross-machine or adversarial scenarios it is not. The roadmap requires a solution that holds
under the latter.

For single-machine deployments, Option A is the correct current implementation (Step 2 above).
This ADR does not remove it; it designates it as the degenerate local case.

### Option B: Per-lambda namespace partitions in the shared SQLite store

Give each lambda a dedicated namespace (e.g., `lambda:khive`, `lambda:leo`) in the shared
SQLite file and use namespace routing to scope inboxes. ADR-057 §Alternatives (A2) rejected
this approach: it orphans the existing shared corpus, requires every lambda to configure
`visible_namespaces` for cross-pack access, and imposes a non-trivial migration for existing
deployments. The operational burden is high and the approach does not solve cross-machine
delivery anyway.

Rejected. The correct fix for the local case is Step 1 (actor identity in the token, not
per-lambda namespace partitions). The correct fix for cross-machine is Step 3 (remote broker).

### Option C: Amend ADR-007 globally to permit per-pack namespace carry for comm

Reopen the ADR-007 Rev 3 "all packs no-carry" ruling specifically for the comm pack, allowing
comm to use the actor namespace as the storage namespace for message rows.

Rejected. ADR-007 Rev 3 was a deliberate ruling by Ocean after a gemini REFUTE review that
found per-pack actor routing to be a contradiction of Rule 0. Reopening it for one pack would
require re-arguing the same ground and would leave the ruling unstable. The Rule 8 carve-out
is the correct mechanism: it permits a different backend, not a different namespace routing
rule for the shared substrate.

### Option D: Wait for ADR-053 ActorStore before specifying the comm isolation model

Defer this ADR until ADR-053 (ActorStore, SessionStore, cloud-tier actor threading) is fully
implemented and ratified.

Rejected. ADR-053 is a prerequisite for the remote broker (it specifies the authenticated
actor model the broker credential builds on), but the comm isolation model can be specified
now. Specifying it now gives issue #75 a clear target and prevents the inbox leak from being
treated as a permanent state.

---

## Open Questions

### OQ-1: Authentication mechanism for the remote broker

The credential format (token type, rotation policy, revocation mechanism) for the remote
broker is unspecified. This is intentional: the credential is part of the transport layer,
which will be addressed in a dedicated transport ADR before Step 3 implementation begins.
The question for Ocean's ruling is whether the credential should be a symmetric pre-shared
key per lambda pair, a public-key credential per lambda, or a centrally-issued token (e.g.,
from a khive-cloud authority). Decision needed before any Step 3 implementation PR is opened.

### OQ-2: Whether partition = separate DB per principal vs principal-column with mandatory scope

In the remote broker backend, messages destined for `lambda:leo` must be readable only by
`lambda:leo`. Two storage approaches are possible:

- **Separate DB per principal**: each lambda has its own store (file, table set, or
  broker-native partition). No cross-principal row can ever appear in the same store.
- **Shared store with mandatory server-side scope**: a single store carries a
  `principal_id` column; every read is wrapped in a `WHERE principal_id = $caller` that
  the server enforces (not a client-side filter). The server rejects queries that omit
  the scope clause.

The correct choice depends on the broker technology selected for Step 3. This is left to the
transport ADR. The isolation contract (per-principal scope, server-enforced) is specified here;
the physical layout is not.

### OQ-3: Legacy message visibility under Step 2

When Step 1 lands and the `to_actor` filter activates, messages written before Step 1 have no
`to_actor` field (or have `to_actor="local"` from the stub label). ADR-057 Q3 noted this:
the `EqOrMissing` filter makes such messages visible to any caller whose actor label matches
OR where the field is absent. Whether existing party-line messages should be backfilled with
`to_actor="local"` (to make them visible in single-actor fallback inboxes) or declared
out-of-scope for actor-scoped inboxes requires Ocean's decision before the Step 2 migration
story is finalized. This is the same open question as ADR-057 Q3; it is repeated here because
its answer gates the data migration plan for Step 2.

---

## References

- ADR-007 Rev 7, Rule 8 — the carve-out this ADR invokes.
- ADR-028 — Pack-Scoped Backends; the deferred multi-backend configuration mechanism.
- ADR-040 — Communication and Schedule Packs; the original comm verb surface.
- ADR-053 — ActorStore, SessionStore (Proposed); actor identity threading, prerequisite for
  Step 1 and Step 3.
- ADR-056 — Channel Transport Layer; the sibling human out-of-band transport. Distinct from
  this ADR: ADR-056 bridges external channels (Telegram, email) to the comm verb surface;
  this ADR specifies the lambda-to-lambda isolation contract behind that same surface.
- ADR-057 — Comm Actor-Addressed Delivery (Accepted); the forward-deployed `to_actor` filter
  that Step 2 activates. ADR-057 Q3 is an open question for this ADR's Step 2 migration plan.
- Issue #75 — Actor identity on every request; Step 1.
- Issue #112 — khive-channel umbrella; Step 3 transport work.
