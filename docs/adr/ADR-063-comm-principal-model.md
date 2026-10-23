# ADR-063: Comm Pack Principal Model and Remote Backend Isolation

**Status**: Proposed
**Date**: 2026-06-19
**Authors**: khive maintainers
**Depends on**: ADR-007 Rev 7 (namespace carve-out for principal-scoped backends) |
ADR-028 (Pack-Scoped Backends) | ADR-040 (Communication and Schedule Packs) |
ADR-057 (Comm Actor-Addressed Delivery) | ADR-127 (Authenticated Actor and Grant Primitive) |
ADR-129 (Fail-Closed Authorization Gate Default)
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

The facts below were refreshed on 2026-10-23 at commit `8f2f867d` (Amendment 1, Decision 3).
This ADR was drafted on 2026-06-19 against a codebase that lacked actor identity plumbing.
Both Step 1 and Step 2 below have since shipped. This section describes the state at commit
`8f2f867d`, superseding the original "Current State / Bug" framing that described a still-open issue
#75. The Decision and Alternatives sections were authored against the pre-fix state and
are retained below for their design rationale; the Migration and Sequencing section (§5) has
been updated to mark Steps 1-2 as shipped.

Issue #75 landed in commit `f1061d27` ("thread authenticated actor identity into request
token"). `RuntimeConfig` (`crates/khive-runtime/src/config.rs:443`) now carries:

```rust
pub actor_id: Option<String>,
```

populated from the `KHIVE_ACTOR` environment variable (`crates/khive-runtime/src/config.rs:570-572`) or
`[actor] id` in `khive.toml` (`config.rs:1108-1118`). At token-mint time, the runtime resolves the actor
from this field (`crates/khive-runtime/src/runtime.rs:1707-1708, 1763-1768`). `resolve_actor` maps a
non-empty id to a named actor and empty or missing input to an anonymous actor
(`crates/khive-runtime/src/actor_identity.rs:18-22`):

```rust
resolve_actor(self.config.actor_id.as_deref())
```

`ActorRef::anonymous()` (`crates/khive-gate/src/actor.rs:65`) still resolves to
`id: "local"` when no actor is configured, so the undecorated single-actor deployment keeps
its original behavior. The comm handlers no longer derive the caller's actor label from the
namespace string; they read it directly off the token:

**`crates/khive-pack-comm/src/handlers/inbox.rs`, `handle_inbox` (lines 11-13, 174-192):**

```rust
let include_legacy = !view.delegated && caller_inherits_legacy_pool(token);
```

**`crates/khive-pack-comm/src/handlers.rs`, `handle_send` (lines 141-143):**

```rust
let from_actor = token.actor().id.clone();
```

Sender attribution (`from_actor`) is effective for deployments that configure
`actor_id`; it uses the anonymous `"local"` identity only when no actor is configured.

Commit `091231cd` ("close anonymous inbox read leak and warn on unattributed addressed send",
issues #199/#200, PR #213) changed the inbox visibility rule.
The `to_actor` filter in `handle_inbox` uses an indexed exact-recipient filter for named and
delegated views, and a legacy-recipient filter only for the anonymous fallback
(`crates/khive-pack-comm/src/handlers/inbox.rs:13,174-192`):

The predicate is selected as `EqOrMissingIndexed` when `include_legacy` is false and
`EqOrLegacyIndexed` when it is true; the actor id comes from the authorized mailbox view.

Only the anonymous fallback inherits rows with missing or null `to_actor`; named actors require an exact
addressed recipient. ADR-057 Q3 states this rule
(`docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`), and the comm handlers implement it
(`crates/khive-pack-comm/src/handlers/validation.rs:265-273`, `handlers/inbox.rs:13,174-192`). This
closes the #199 multi-actor read leak described in this ADR's original "Current State / Bug" section.

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
party-line messages, closing the multi-actor read leak that motivated this ADR. (superseded by Amendment 1, Decision 1)

At commit `8f2f867d`, only the anonymous fallback inherits messages with missing or null `to_actor`;
named actors require an exact addressed recipient (corrected by Amendment 1).

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
and 2 are SHIPPED (fact-refreshed 2026-10-23); only Step 3 remains future work.

**Step 1 — Actor identity plumbing (issue #75, prerequisite for both Step 2 and Step 3):
SHIPPED (commit `f1061d27`).**

The `NamespaceToken` now carries a distinct, non-anonymous `ActorRef` per lambda when
configured. `RuntimeConfig` gained an `actor_id: Option<String>` field
(`crates/khive-runtime/src/config.rs:437-443`), populated from `KHIVE_ACTOR` and `[actor] id`
(`config.rs:570-572,1108-1118`) and
resolved at token authorization (`crates/khive-runtime/src/runtime.rs:1707-1708,1763-1768`,
`crates/khive-runtime/src/actor_identity.rs:18-22`).

Changes landed as Step 1:

- `RuntimeConfig`: `actor_id: Option<String>` (`crates/khive-runtime/src/config.rs:437-443`),
  populated from `KHIVE_ACTOR` and `[actor] id` (`config.rs:570-572,1108-1118`).
- Token authorization resolves the actor from `actor_id` (`crates/khive-runtime/src/runtime.rs:1707-1708,1763-1768`).
- `comm.send` reads sender identity from `token.actor().id`
  (`crates/khive-pack-comm/src/handlers.rs:141-143`). Inbox recipient visibility is selected from the
  authorized mailbox view and anonymous legacy-pool predicate
  (`crates/khive-pack-comm/src/handlers/inbox.rs:11-13,174-192`,
  `crates/khive-pack-comm/src/handlers/validation.rs:265-273`).

Step 1 shipped as shared infrastructure, not comm-specific — the actor identity is available
for attribution across all packs. ADR-053 is superseded and retained as historical context
(corrected by Amendment 1, Decision 2; `docs/adr/ADR-053-authorization-gate.md:3-8,16-27`).

**Step 2 — Local inbox scope (fix the leak): SHIPPED (commit `091231cd`, PR #213, issues
#199/#200).**

The `comm.inbox` filter uses the authorized mailbox view and an indexed `to_actor` predicate. Named actors require exact
addressed recipients; only the anonymous fallback inherits rows with missing or null `to_actor`
(corrected by Amendment 1, Decision 1; `crates/khive-pack-comm/src/handlers/inbox.rs:11-13,174-192`,
`crates/khive-pack-comm/src/handlers/validation.rs:265-273`,
`docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`).

This resolves the inbox leak for the single-machine deployment. The security model remains
the local view-layer filter (not a security boundary for adversarial principals, but adequate
for co-located, cooperating lambdas), per Decision §1a above.

**Step 3 — Remote broker backend (cross-machine delivery):**

The remote broker backend is a future implementation. It requires:

- An authentication mechanism per lambda (credential details are addressed in Amendment 1, Decision 2).
- A broker implementation that enforces per-principal scope server-side.
- Pack-scoped backend configuration (ADR-028 multi-backend machinery or an equivalent; see Amendment
  1, Decision 2 for its relationship to ADR-105).
- The same `comm.*` verb surface, unchanged. Callers do not change; only the backend changes.

Step 1 has shipped. The former ADR-053 prerequisite is superseded by ADR-127 and ADR-129. Step 3
remains gated by the questions recorded in Amendment 1, Decision 2. It was
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
implemented and ratified. (superseded by Amendment 1, Decision 2)

Rejected. ADR-053 is a prerequisite for the remote broker (it specifies the authenticated
actor model the broker credential builds on), but the comm isolation model can be specified
now. Specifying it now gives issue #75 a clear target and prevents the inbox leak from being
treated as a permanent state. (superseded by Amendment 1, Decision 2)

---

## Open Questions

### OQ-1: Authentication mechanism for the remote broker

The credential format (token type, rotation policy, revocation mechanism) for the remote
broker is unspecified. This is intentional: the credential is part of the transport layer,
which will be addressed in a dedicated transport ADR before Step 3 implementation begins
(corrected by Amendment 1, Decision 2: ADR-127 defines authentication and principal assurance
without this broker's credential format, rotation or revocation, and ADR-105's per-node bearer
covers its node channel; whether either governs this backend remains open).
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
transport ADR (corrected by Amendment 1, Decision 2: ADR-105, the transport record, does not
define the broker storage partition, so the choice remains open). The isolation contract
(per-principal scope, server-enforced) is specified here; the physical layout is not.

### OQ-3: Legacy message visibility under Step 2 — RESOLVED as shipped (commit `091231cd`, PR #213)

This was originally posed as an open question for maintainer decision (same question as ADR-057
Q3): whether legacy messages without a `to_actor` field should be backfilled with
`to_actor="local"` or declared out-of-scope for actor-scoped inboxes. It was resolved by
implementation rather than by a separate backfill decision. The shipped `EqOrMissing` filter
(`handlers.rs:188-192`) makes messages with no `to_actor` field visible to any caller whose
actor label matches the filter value, including the anonymous `"local"` caller (corrected by
Amendment 1, Decision 1: only the anonymous fallback inherits messages with a missing or null
`to_actor`, and a configured actor sees only messages addressed to it;
`crates/khive-pack-comm/src/handlers/inbox.rs:13,174-192`). No backfill
migration was performed or is required: legacy messages remain visible under the
`EqOrMissing` semantics as originally implemented in ADR-057, and PR #213's unconditional
filter (see "Current State" above) extends the same semantics to the anonymous path
(superseded by Amendment 1, Decision 1; ADR-057's compatibility view does not rewrite legacy
records, `docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`). This
question is closed; no further decision is pending on it.

---

## References

- ADR-007 Rev 7, Rule 8 — the carve-out this ADR invokes.
- ADR-028 — Pack-Scoped Backends; the deferred multi-backend configuration mechanism.
- ADR-040 — Communication and Schedule Packs; the original comm verb surface.
- ADR-053: Authorization Gate; superseded and retained for history (corrected by Amendment 1, Decision 2).
- ADR-127: Authenticated actor and grant primitive; credential and principal decisions relevant to Step 3
  (see Amendment 1, Decision 2).
- ADR-129: Fail-closed authorization gate default; authorization gate decisions relevant to Step 3
  (see Amendment 1, Decision 2).
- ADR-105: Cross-node comm transport; hub topology and node transport (see Amendment 1, Decision 2).
- ADR-056 — Channel Transport Layer; the sibling human out-of-band transport. Distinct from
  this ADR: ADR-056 bridges external channels (Telegram, email) to the comm verb surface;
  this ADR specifies the lambda-to-lambda isolation contract behind that same surface.
- ADR-057 — Comm Actor-Addressed Delivery (Accepted); the `to_actor` filter
  used by Step 2. Q3 was revised on 2026-09-22 (see
  `docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`, corrected by Amendment 1).
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

## Amendment 1 (2026-10-23) -- Actor visibility and remote transport references

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#5549](https://github.com/ohdearquant/khive/issues/5549).

**Trigger.** The local inbox rule in §1a and OQ-3 says configured actors inherit legacy messages, while the current
ADR-057 rule and inbox implementation grant the missing/null recipient pool only to the anonymous fallback
(`docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`,
`crates/khive-pack-comm/src/handlers/inbox.rs:13,174-192`). Step 3 also depends on ADR-053, which is superseded,
and leaves credential and hub layout decisions pointing to an unnamed transport record
(`docs/adr/ADR-053-authorization-gate.md:3-8,20-27`).

### Decision 1 -- Legacy inbox visibility

The local inbox rule is the ADR-057 Q3 rule: only the anonymous fallback inherits messages with missing or null
`to_actor`; named actors require exact addressed recipients. This preserves legacy message visibility for the
anonymous compatibility path without granting that pool to configured actors. ADR-057 Q3 states this rule
(`docs/adr/ADR-057-comm-actor-addressed-delivery.md:488-503`); `caller_inherits_legacy_pool` identifies the
anonymous `local` actor (`crates/khive-pack-comm/src/handlers/validation.rs:265-273`), and `handle_inbox` uses
that predicate to select the indexed recipient filter (`crates/khive-pack-comm/src/handlers/inbox.rs:11-13,174-192`).

### Decision 2 -- Step 3 decision references and remaining questions

Step 3 references ADR-127 for authenticated principal and credential decisions, and ADR-129 for authorization
gate administration. ADR-053 is superseded: its own supersession note assigns authentication to ADR-127 and
multi-actor authorization to ADR-129 (`docs/adr/ADR-053-authorization-gate.md:3-8,20-27`). ADR-127 requires
caller authentication from a presented credential and classifies its assurance
(`docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:62-88`); ADR-129 specifies a staged fail-closed
authorization gate (`docs/adr/ADR-129-fail-closed-gate-default.md:82-106`). These records do not specify the
broker-specific token format, rotation, or revocation contract for this ADR. ADR-105 specifies a hub-and-spoke
node channel and per-node bearer over TLS (`docs/adr/ADR-105-cross-node-comm-transport.md:42-44,116-123`), but
that transport's relationship to this ADR's backend swap remains open. ADR-105 says `comm.send`, `comm.reply`,
and dual-write are untouched (`docs/adr/ADR-105-cross-node-comm-transport.md:1226-1230`), so it does not choose
between a separate broker backend and that node channel.

OQ-1 points to ADR-127 for the authenticated-principal and credential model. The concrete broker credential
format and lifecycle remain open because ADR-127 does not specify them for this backend. The per-node bearer
selected by ADR-105 applies to its node channel; whether it also supplies Step 3's backend credential remains
open (`docs/adr/ADR-105-cross-node-comm-transport.md:116-123`).

OQ-2 points to ADR-105 for hub and transport topology. ADR-105 specifies the star topology and hub ingress
(`docs/adr/ADR-105-cross-node-comm-transport.md:42-44,74-84`), but does not choose the remote broker's storage
partition. The choice between separate storage per principal and a shared store with mandatory server-side
scope remains open. The relationship between ADR-105's node channel and this ADR's pack backend also remains
open; ADR-105 preserves dual-write (`docs/adr/ADR-105-cross-node-comm-transport.md:1226-1230`).

The relationship to ADR-028 also remains open. ADR-028 says `[[backends]]` and per-pack backend assignment
are deferred (`docs/adr/ADR-028-pack-scoped-backends.md:51-63`), while the checkout has closed `[[backends]]`
config entries and a multi-backend server boot path (`crates/khive-runtime/src/engine_config.rs:1402-1403`,
`crates/khive-mcp/src/serve/server_build.rs:677-688`); the namespace-move module describes comm notes on a
separate backend (`crates/khive-db/src/namespace_move.rs:13-20`). This record does not resolve
how this affects ADR-063's backend dependency, so that relationship remains open. Step 3 implementation
waits on the OQ-1 broker credential format and lifecycle, the OQ-2 storage partition, and the
relationship between ADR-105's node channel and this pack backend.

### Decision 3 -- Current state and citation refresh

Current State and §5 describe the checkout at `8f2f867d` as of 2026-10-23. Their actor configuration, token
authorization, sender attribution, and inbox visibility citations are refreshed to the current source locations
(`crates/khive-runtime/src/config.rs:437-443,570-572,1108-1118`,
`crates/khive-runtime/src/runtime.rs:1707-1708,1763-1768`,
`crates/khive-runtime/src/actor_identity.rs:18-22`,
`crates/khive-pack-comm/src/handlers.rs:141-143`,
`crates/khive-pack-comm/src/handlers/inbox.rs:11-13,174-192`,
`crates/khive-db/src/stores/note.rs:1117-1121,1176-1185`). ADR-053 is labeled superseded in its own status
and supersession note (`docs/adr/ADR-053-authorization-gate.md:3-8,10-27`).

### Acceptance

| ID  | Statement                                                                                                                                                 | Evidence                                                                                                                                                      |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1a | Named actors receive only messages addressed to that actor, while the anonymous fallback also receives missing or null `to_actor` rows.                   | `crates/khive-pack-comm/tests/integration.rs::i1739_named_own_views_do_not_inherit_the_anonymous_pool`                                                        |
| A1b | The inbox selects the indexed exact-recipient predicate for named and delegated views and the legacy-recipient predicate only for the anonymous fallback. | `crates/khive-pack-comm/src/handlers/inbox.rs:11-13,174-192`                                                                                                  |
| A2a | Runtime actor identity accepts `KHIVE_ACTOR` and `[actor] id`, then resolves configured ids to named actors.                                              | `crates/khive-runtime/src/config.rs:437-443,570-572,1108-1118`; `crates/khive-runtime/src/actor_identity.rs:18-22`                                            |
| A2b | Comm sender attribution uses the actor id carried by the request token.                                                                                   | `crates/khive-pack-comm/src/handlers.rs:141-143`                                                                                                              |
| A3  | ADR-053 assigns authentication decisions to ADR-127 and multi-actor authorization decisions to ADR-129.                                                   | `docs/adr/ADR-053-authorization-gate.md:3-8,20-27`                                                                                                            |
| A4  | ADR-127 defines credential-based caller authentication and assurance classes, but leaves this broker's credential format and lifecycle open.              | `docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:62-88`                                                                                           |
| A5a | ADR-105 specifies hub topology and a per-node bearer over TLS, while its dual-write rule does not select this ADR's backend.                              | `docs/adr/ADR-105-cross-node-comm-transport.md:42-44,116-123,1226-1230`                                                                                       |
| A5b | OQ-1 leaves the broker credential format and lifecycle open, and OQ-2 leaves the storage partition open.                                                  | `docs/adr/ADR-063-comm-principal-model.md:374-395`                                                                                                            |
| A6a | ADR-028 defers `[[backends]]` and per-pack backend assignment, while the checkout parses `[[backends]]` and boots a multi-backend server.                 | `docs/adr/ADR-028-pack-scoped-backends.md:51-63`; `crates/khive-runtime/src/engine_config.rs:1402-1428`; `crates/khive-mcp/src/serve/server_build.rs:677-688` |
| A6b | The namespace-move module describes comm notes on a separate backend but does not resolve ADR-063's backend dependency.                                   | `crates/khive-db/src/namespace_move.rs:13-20`                                                                                                                 |
