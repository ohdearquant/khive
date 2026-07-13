# ADR-063: Comm Pack Principal Model and Remote Backend Isolation

**Status**: Proposed
**Date**: 2026-06-19
**Authors**: khive maintainers
**Depends on**: ADR-007 Rev 7 (namespace carve-out for principal-scoped backends) |
ADR-028 (Pack-Scoped Backends) | ADR-040 (Communication and Schedule Packs) |
ADR-057 (Comm Actor-Addressed Delivery) | ADR-053 (ActorStore / SessionStore — pending)
**Related issues**: #75 (actor identity on every request) | #112 (khive-channel umbrella) |
#113 (Telegram adapter)
**Related ADRs**: ADR-056 (Channel Transport Layer — human out-of-band; sibling, not the same)

---

## Context

_(Historical: this Context section, and the "Current State / Bug" code excerpt originally
following it, describe the codebase as of 2026-06-19, before issue #75 and PR #213 shipped.
See "Current State (fact-refreshed 2026-07-04)" below for the shipped behavior. Retained here
for the design rationale that motivated the Decision section.)_

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

Cross-machine lambda-to-lambda communication is a roadmap item: lambdas
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

## Current State (fact-refreshed 2026-07-04 against shipped `main`)

This ADR was drafted on 2026-06-19 against a codebase that lacked actor identity plumbing.
Both Step 1 and Step 2 below have since shipped. This section describes the state as of
`main`, superseding the original "Current State / Bug" framing that described a still-open
issue #75. The Decision and Alternatives sections were authored against the pre-fix state and
are retained below for their design rationale; the Migration and Sequencing section (§5) has
been updated to mark Steps 1-2 as shipped.

Issue #75 landed in commit `f1061d27` ("thread authenticated actor identity into request
token"). `RuntimeConfig` (`crates/khive-runtime/src/config.rs:267`) now carries:

```rust
pub actor_id: Option<String>,
```

populated from the `KHIVE_ACTOR` environment variable or `[actor] id` in `khive.toml`
(`config.rs:316-318`, `config.rs:479`). At token-mint time (`crates/khive-runtime/src/pack.rs`
and `runtime.rs:423-425`), the runtime builds the `ActorRef` from this field:

```rust
let actor = match self.config.actor_id.as_deref() {
    Some(id) if !id.trim().is_empty() => ActorRef::new("actor", id),
    _ => ActorRef::anonymous(),
};
```

`ActorRef::anonymous()` (`crates/khive-gate/src/actor.rs:65`) still resolves to
`id: "local"` when no actor is configured, so the undecorated single-actor deployment keeps
its original behavior. The comm handlers no longer derive the caller's actor label from the
namespace string; they read it directly off the token:

**`crates/khive-pack-comm/src/handlers.rs`, `handle_inbox` (line 154):**

```rust
let caller_actor = token.actor().id.clone();
```

**`crates/khive-pack-comm/src/handlers.rs`, `handle_send` (line 77):**

```rust
let from_actor = token.actor().id.clone();
```

Sender attribution (`from_actor`) is therefore effective for any deployment that configures
`actor_id`; it degrades to `"local"` only in the undecorated default, which is the documented
degenerate case, not a bug.

Commit `091231cd` ("close anonymous inbox read leak and warn on unattributed addressed send",
issues #199/#200, PR #213) then removed the `if caller_actor != "local"` conditional entirely.
The `to_actor` filter in `handle_inbox` is now unconditional
(`crates/khive-pack-comm/src/handlers.rs:188-192`):

```rust
property_filters.push(PropertyFilter {
    json_path: "$.to_actor".to_string(),
    op: FilterOp::EqOrMissing,
    value: SqlValue::Text(caller_actor.clone()),
});
```

For an anonymous caller (`caller_actor == "local"`), this applies `EqOrMissing("local")`
instead of skipping the filter, so the caller sees only party-line messages (`to_actor` equal
to `"local"` or absent). This closes the #199 multi-actor read leak described in this ADR's
original "Current State / Bug" section: an anonymous caller can no longer read messages
explicitly addressed to a different configured actor.

This state supersedes the "dormant until issue #75 lands" framing throughout the rest of this
document. Where later sections describe the filter as forward-deployed-but-inactive, read
that as historical context for the design decision, not the current runtime behavior.

---

## Decision

### 1. Principal model

For the purposes of the comm pack, a **principal** is the entity that sends and receives
messages. In the current OSS deployment, the principal is identified by its actor label. In a
remote broker deployment, the principal is identified by its authenticated credential.

The principal model has two implementations, selected by the backend the comm pack uses:

**1a. Local SQLite backend (current, degenerate):**

Principal identity is the actor label derived from the runtime context. As shipped
(`f1061d27`, `091231cd` — see "Current State" above), the actor label is `token.actor().id`,
sourced from `RuntimeConfig.actor_id` (configured via `KHIVE_ACTOR` / `[actor] id`) and
falling back to `ActorRef::anonymous()`'s `"local"` id when unconfigured. The per-actor inbox
filter (`to_actor`) is active unconditionally: configured actors see only messages addressed
to them (plus legacy messages with no `to_actor`), and anonymous (`"local"`) callers see only
party-line messages, closing the multi-actor read leak that motivated this ADR.

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

The work to close the inbox leak and enable remote delivery proceeds in three steps. Steps 1
and 2 are SHIPPED (fact-refreshed 2026-07-04); only Step 3 remains future work.

**Step 1 — Actor identity plumbing (issue #75, prerequisite for both Step 2 and Step 3):
SHIPPED (commit `f1061d27`).**

The `NamespaceToken` now carries a distinct, non-anonymous `ActorRef` per lambda when
configured. `RuntimeConfig` gained an `actor_id: Option<String>` field
(`crates/khive-runtime/src/config.rs:267`), populated from `KHIVE_ACTOR` / `[actor] id` and
threaded through the token-mint sites in `runtime.rs` and `pack.rs` into the `ActorRef`.

Changes landed as Step 1:

- `RuntimeConfig`: `actor_id: Option<String>` (`config.rs:267`).
- Token-mint sites (`runtime.rs:423-425`, `pack.rs:854-856, 898-900, 990-992`): populate
  `ActorRef::new("actor", id)` from `actor_id` when configured, else `ActorRef::anonymous()`.
- `comm` handlers: derive actor label from `token.actor().id` directly
  (`handlers.rs:77, 154`) rather than from `token.namespace().as_str()`.

Step 1 shipped as shared infrastructure, not comm-specific — the actor identity is available
for attribution across all packs, per the original design intent (ADR-053).

**Step 2 — Local inbox scope (fix the leak): SHIPPED (commit `091231cd`, PR #213, issues
#199/#200).**

The `comm.inbox` filter is active: `caller_actor` is `token.actor().id`, and messages
addressed to a specific configured actor are returned only to that actor. PR #213 additionally
made the `to_actor` filter unconditional (removed the `if caller_actor != "local"` gate), so
even anonymous (`"local"`) callers now get an explicit `EqOrMissing("local")` scope rather than
an unfiltered read — closing the #199 multi-actor read leak on the anonymous path as well.

This resolves the inbox leak for the single-machine deployment. The security model remains
the local view-layer filter (not a security boundary for adversarial principals, but adequate
for co-located, cooperating lambdas), per Decision §1a above.

**Step 3 — Remote broker backend (cross-machine delivery):**

The remote broker backend is a future implementation. It requires:

- An authentication mechanism per lambda (credential format TBD in a transport ADR).
- A broker implementation that enforces per-principal scope server-side.
- Pack-scoped backend configuration (ADR-028 multi-backend machinery or an equivalent).
- The same `comm.*` verb surface, unchanged. Callers do not change; only the backend changes.

Step 1 has shipped, satisfying that prerequisite. Step 3 remains blocked on the transport ADR
(which specifies the credential format and broker protocol) and is otherwise unblocked. It was
never blocked on Step 2; the local fix and remote broker can be developed independently.

### 6. Pack verb surface

The `comm.*` verb surface is unchanged by this ADR. `comm.send`, `comm.inbox`, `comm.reply`,
`comm.thread`, and `comm.read` work identically against the local SQLite backend and the
remote broker backend. The backend is an implementation detail of the comm pack; callers
observe the same behavior.

---

## Consequences

### Positive

- The party-line inbox leak had a documented root cause (missing actor identity in the token)
  and a clear fix path (Step 1 above), rather than being an architectural ambiguity; both the
  root-cause fix (Step 1, `f1061d27`) and the leak closure (Step 2, `091231cd`) have shipped.
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
- Step 1 (issue #75) spanned multiple packs and the runtime dispatch path; this has shipped
  (`f1061d27`), so this is no longer an open dependency for Step 2. Step 3 remains dependent on
  the transport ADR (OQ-1).

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

Rejected. ADR-007 Rev 3 was an accepted design decision that
found per-pack actor routing to be a contradiction of Rule 0. Reopening it for one pack would
require re-arguing the same ground and would leave the decision unstable. The Rule 8 carve-out
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
The open question is whether the credential should be a symmetric pre-shared
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

### OQ-3: Legacy message visibility under Step 2 — RESOLVED as shipped (commit `091231cd`, PR #213)

This was originally posed as an open question for maintainer decision (same question as ADR-057
Q3): whether legacy messages without a `to_actor` field should be backfilled with
`to_actor="local"` or declared out-of-scope for actor-scoped inboxes. It was resolved by
implementation rather than by a separate backfill decision. The shipped `EqOrMissing` filter
(`handlers.rs:188-192`) makes messages with no `to_actor` field visible to any caller whose
actor label matches the filter value, including the anonymous `"local"` caller. No backfill
migration was performed or is required: legacy messages remain visible under the
`EqOrMissing` semantics as originally implemented in ADR-057, and PR #213's unconditional
filter (see "Current State" above) extends the same semantics to the anonymous path. This
question is closed; no further decision is pending on it.

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
  that Step 2 activates. ADR-057 Q3 was resolved as shipped by PR #213 (see OQ-3 above).
- Issue #75 — Actor identity on every request; Step 1. SHIPPED, commit `f1061d27`.
- Issue #112 — khive-channel umbrella; Step 3 transport work.
- PR #213 (issues #199/#200) — closed the anonymous inbox read leak by making the `to_actor`
  filter unconditional; commit `091231cd`. Step 2 as shipped.
- Issue #448 / PR #496 (commit `4943dbea`) — inbound email attribution on the channel-email
  connector now requires an authenticated identity, not just a matching From header. Adjacent
  hardening on the same actor-attribution surface this ADR describes; no design change to the
  principal model here.
- Commit `f816c3e6` (PR #526) — dual-write rollback atomicity fix for comm message writes.
  Adjacent hardening on the comm pack's write path; no design change to the principal model
  here.
