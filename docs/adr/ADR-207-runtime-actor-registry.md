# ADR-207: Runtime Actor Registry and Authenticated Operation Context

- **Status:** Proposed
- **Date:** 2026-10-20
- **Relates to:** ADR-007 (namespace attribution and store isolation), ADR-018 (dispatch authorization), ADR-125 (reserved
  property keys), ADR-127 (authenticated actor and grant primitive), ADR-129 (fail-closed gate default). ADR-053 is
  cited for history only.
- **Amends, when accepted:** ADR-127 (a host-verified assurance class, D5), ADR-129 Amendment 4 (resolved actor
  arguments before admission, D7), ADR-007 (registered-mode routing without the implicit `local` expansion, D5).

## Context

This record serves the maintainer's decision of 2026-10-16 on multi-actor accounts for a hosted embedder; the
embedder owns the product specification, and this record is the runtime data model under it.

A hosted embedder of the runtime lets one account act as several named actors. Each actor has a default project. The
account picks the actor when it authorizes a connector, and the choice is bound into the credential the connector
then presents. The runtime has to answer four questions for that embedder that it cannot answer today: which actors
exist in a store, which actor an operation runs as, which project it writes to, and what an asynchronous effect is
attributed to after the request that caused it has returned.

What the runtime has today, at `a1a60f3f`:

- Actor labels are opaque attribution strings, not allocated identities. `ActorRef` has a structural kind and id and
  a stamped-label splitter (`crates/khive-gate/src/actor.rs:15`, `:57`, `:104`), and `resolve_actor` wraps an
  entire untrimmed label as kind `actor` (`crates/khive-runtime/src/actor_identity.rs:18`). Nothing allocates a label,
  checks it for uniqueness or prevents its reuse.
- `RUNTIME_STAMPED_ACTOR_KINDS` holds `actor`, `anonymous` and `agent` (`crates/khive-gate/src/actor.rs:9`). It is not a
  reservation set: the runtime also stamps `daemon:checkpoint_task` (`crates/khive-db/src/checkpoint/lifecycle.rs:261`)
  and `system:propose-apply` (`crates/khive-pack-kg/src/apply_worker/worker.rs:774`), and an inbound reply
  opt-out on a runtime with no configured actor records its suppression under `channel:email`
  (`crates/khive-pack-comm/src/handlers/ingest.rs:554`).
  Comm recognizes `email:`, `telegram:` and `khive1:` as outbound address prefixes
  (`crates/khive-pack-comm/src/file_attachments.rs:49`), and the outbox delivers the email and telegram channels
  (`crates/khive-mcp/src/serve_outbox.rs:156`).
- A dispatch carries one actor. `NamespaceToken` holds one `ActorRef` (`crates/khive-runtime/src/config.rs:133`),
  `RequestIdentity` one optional label (`crates/khive-runtime/src/pack/request_identity.rs:117`, `:125`), and
  `GateContext` session, time and source only (`crates/khive-gate/src/context.rs:6`). Nothing separates the
  credential's principal, the human behind it and the actor it acts as.
- ADR-127's authenticated context is not implemented. `VerifiedActor` and `dispatch_as` exist
  (`crates/khive-runtime/src/pack/request_identity.rs:178`; `crates/khive-runtime/src/pack/dispatch.rs:1326`), but the
  wrapper checks only that the label is not blank; authentication belongs to the embedding caller.
- No `tenant_id` appears in `khive-runtime`, `khive-types` or the `khive-db` SQL. ADR-007 makes tenant isolation the
  embedder's job and names one store per tenant as the supported shape (`docs/adr/ADR-007-namespace.md:295`, `:299`).
- Implicit dispatch writes to `local` and widens reads to `local`, the configured visible namespaces, and the
  caller's own actor namespace when it parses and is not `local` (`crates/khive-runtime/src/pack/dispatch.rs:868`,
  `:893`).
- Actor-valued arguments are classified by hand-written verb and field-name lists in three places: the gate's
  mailbox selector table (`crates/khive-gate/src/mailbox.rs:48`), the runtime's mailbox view
  (`crates/khive-runtime/src/mailbox_view.rs:110`), and the delegation check's verb list
  (`crates/khive-gate/src/mailbox.rs:188`). Elsewhere the meaning lives in handlers: brain defaults a binding read to
  the caller (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:1758`) and checks a named foreign actor
  (`:224`), while `brain.bind` turns an omitted actor into the wildcard `*` and checks no named actor against the
  caller (`:1553`), and `brain.unbind` with no actor matches every actor's bindings (`:1716`). Comm takes the sender
  from the token and the recipient from input (`crates/khive-pack-comm/src/handlers.rs:142`). `ParamDef` carries no
  identity or address meaning (`crates/khive-types/src/pack.rs:166`).

This record assumes the hosted embedder routes each tenant to its own store, the shape ADR-007 supports. The runtime
cannot observe that routing; activation prerequisite 1 requires the embedder to show it, and D2 says what a
shared-store deployment would still need.

## Decision

### D1. The runtime owns a non-graph actor registry in the tenant's primary store

A dedicated actor table family lives in the store the embedder designates as the tenant's primary store, written
through the existing writer path. It is not a graph entity or note: generic curation and caller-supplied
properties must not become allocation or lifecycle authority (ADR-125 distinguishes creation from mutation for the
same reason, `docs/adr/ADR-125-reserved-property-keys.md:98`). It is not the `ActorStore` of ADR-053, whose job was
token-to-caller resolution (`docs/adr/ADR-053-authorization-gate.md:94`) and which ADR-127 carries forward as the
authentication seam.

A record holds an immutable runtime-minted UUID, an immutable canonical handle (D3), an editable display name
(non-blank UTF-8, at most 256 bytes, no control characters), a lifecycle (`active` or `tombstoned`), a default
project id, a positive revision that increases on every change, and an explicit default-actor flag that ordinary
updates cannot move. Looking up an unknown actor never creates one. A tombstone keeps the UUID and handle forever;
neither is ever reused.

Placement follows the current dependency direction: neutral identifiers and record types in `khive-types`; the
handle parser, the reservation set and the identity-versus-address gate inputs in `khive-gate`; an `ActorRegistry`
persistence capability in `khive-storage` with no permission decision; SQL and the SQLite implementation in
`khive-db`; validated management operations and dispatch integration in `khive-runtime`.

The first callable surface is the runtime embedding API: enable, create, resolve by exact UUID or handle, bounded
list, revision-checked update and tombstone. Its first consumer is the embedder's actor-management and consent adapter.
No public management pack ships until a caller needs one; when it does, it is a thin adapter over the same operations.

Each management operation is an authorization seam under ADR-018, which enforces authorization at runtime seams whatever
the composition root (`docs/adr/ADR-018-authorization-gate.md:459`). The call carries a sealed context (D5) naming the
principal that asks; the embedder's adapter acts under its own host-administrative principal, never under a member's
context. That host-administrative context carries no effective actor. Before the store is enabled it is the only context
a management operation accepts; ordinary legacy contexts exist in every mode (D5) and carry no management authority.
Before any registry read or write the runtime asks the gate for a decision on the named operation (`actor.enable`, `actor.inventory`, `actor.create`, `actor.resolve`, `actor.list`, `actor.update`, `actor.tombstone`) for that principal. A denial or an
unavailable decision refuses. A registered-mode composition built with `AllowAllGate`
(`crates/khive-gate/src/gate.rs:55`) fails at construction; this is a registered-mode rule and leaves ADR-129's global
default as it is. The storage capability below the operations makes no permission decision and is not a caller-facing
surface. Listing, the inventory read and lifecycle are administrative operations. A member's view of the actors it may use or address (the
hosted record's visibility rule) is assembled by the embedder from its own permission data, not by a runtime list issued
on the member's behalf; the member learns its own effective actor from its sealed context.

The default designation is a tenant invariant. Exactly one active actor in an enabled store carries it. Enablement is
the only operation that sets it: in the transaction that binds the tenant scope (D2), enablement allocates the default
actor, or links it to one historical identity through D3's evidenced mapping, and records the root project the embedder
names as its default project. Create never sets the designation, update never moves it, and tombstoning the default
actor refuses. The table enforces at most one designated row; a store found enabled with none refuses registered work.
No operation transfers the designation; a product that needs another default is a later decision, not a runtime
capability.

### D2. Tenant scope is trusted input, bound to the store once

Registered mode needs a trusted tenant scope. Enabling the registry binds the designated store to one opaque tenant
UUID, recorded in registry metadata and never changed. Every registry operation carries the scope it expects; a
mismatch refuses. Actor records return the tenant id from the binding, so rows do not repeat a constant tenant
column. A lookup in the wrong scope, a foreign UUID and an absent UUID give the same public not-found result;
mismatch detail goes to operator logs only.

The binding detects a request routed to the wrong store after setup. It does not prove that the store held one tenant's
data before setup, and the runtime does not try to: the embedder's provisioning record is the evidence. Enablement takes
that record as an input (the tenant UUID, the store's physical identity and the attesting operator) and keeps it in
registry metadata. The runtime checks that the record is present and names this store and this scope; it does not check
that it is true. A deployment that shares one store between tenants does not get isolation from an actor-only tenant
column, because entities, messages, tasks and by-ID operations would stay unisolated (ADR-007 explains why a namespace
check in by-ID handlers is not that boundary, `docs/adr/ADR-007-namespace.md:290`, `:311`). Such a deployment can use
the registry for non-hosted work, and hosted activation waits for isolated routing or a separately accepted shared-store
design.

Enablement is bounded to one storage topology. Every row that holds an actor-valued field (D7), other than an event row,
lives in the designated primary store, so the mode marker, the reservation set and those rows share its transactions
(D3). Enablement reads the runtime's storage configuration and refuses unless the designated store is its only storage
backend, apart from a separate events backend (`crates/khive-runtime/src/events_split/events_daemon.rs:628`). That backend
holds the attribution of operations admitted through the designated store and serves history inspection; it holds no
marker and no reservation, the enablement inventory reads it (D3), and its rows without a UUID are legacy (D8). A tenant
whose actor-valued rows span several stores waits for a later decision that states how each store joins the marker and
the reservation set.

### D3. One canonical handle parser for new allocations; legacy labels stay opaque

New allocations go through one parser in `khive-gate`, `CanonicalActorHandle`:

- two or three colon-separated components, `<kind>:<name>[:<qualifier>]`;
- lowercase ASCII only; uppercase input refuses rather than being lowercased;
- kind `[a-z][a-z0-9_-]{0,31}`; name and qualifier `[a-z0-9][a-z0-9_-]{0,63}` each; at most 162 bytes in total;
- no empty component, whitespace, control character, wildcard, extra colon, dot hierarchy or trailing separator,
  and no Unicode normalization.

An allocation refuses a reserved kind, compared case-insensitively. The reservation set is versioned, separate from
`RUNTIME_STAMPED_ACTOR_KINDS`, and starts with `actor`, `agent`, `anonymous`, `channel`, `daemon`, `email`, `khive1`,
`local`, `sess`, `session`, `system` and `telegram`. The hosted embedder's session-principal prefixes are added before
hosted allocation is enabled; this record does not claim the list above covers them. A runtime-produced label is not a
prefix census either: a producer that adds a new system kind adds it to the reservation set in the same change, and a
test asserts that every literal kind the runtime and packs stamp is reserved.

Passing the parser establishes neither registry membership nor permission. `ActorRef` keeps its current behavior,
historical labels are never re-parsed or normalized, and external addresses keep their own validation. Lookups are
typed: a registered UUID or handle on one side, `LegacyIdentity { kind, exact_id }` on the other.

Enablement closes the namespace in this order. First, every write to an actor-valued field in the store starts recording
each legacy label that parses as a canonical handle into a reservation table, in the same transaction as the write, and
so does legacy admission (D5) for its credential's label. Second, an inventory of the history committed before that
point, in the store and in the events backend (D2), and of the configured principals adds to the same table the label
each recorded identity is admitted under, where that label parses. Third, allocation opens. A reserved label cannot be
claimed by a new allocation; an explicit, evidenced mapping may link one historical identity to the default actor (D1),
and an unresolved collision stays reserved. After enablement the rule runs both ways. Legacy ingress refuses a
credential whose label equals a registered handle, live or tombstoned, unless an evidenced mapping links that legacy
identity to that actor (D5), so no legacy principal is admitted under a registered actor's spelling, and because legacy
admission reserves its own label, no actor is allocated later under the spelling of a legacy principal admitted earlier.
A legacy Address value equal to a registered handle names that actor and records its UUID (D5, D8). Every other
parseable legacy label is reserved, unnormalized. One spelling never names two identities, with one bounded exception:
an event that work admitted before enablement appends after the inventory has read the events backend keeps its label as
legacy history (D8) even if that label is allocated later, and is never read as the registered actor's.

The inventory reads each field in its producer's encoding and re-parses no label. An event row's actor value is the
runtime stamp `kind:id` (`crates/khive-runtime/src/pack/audit.rs:71`,
`crates/khive-runtime/src/event_store_guard.rs:43`). A value that splits by the runtime's stamped-kind rule
(`split_stamped_label`, `crates/khive-gate/src/actor.rs:15`) decodes to `LegacyIdentity { kind, exact_id }`. A value
that does not split has three origins at this revision: a system stamp the runtime writes as a literal string, such as
`daemon:checkpoint_task` and `system:propose-apply` above; an id an older producer wrote bare; and the stamp of a
credential whose kind is outside `RUNTIME_STAMPED_ACTOR_KINDS`, which `ActorRef` accepts because its kind set is open
(`crates/khive-gate/src/actor.rs:8`); the runtime's writer formula stamps `service:worker` for a token of kind `service`
and id `worker` (`crates/khive-runtime/src/event_store_guard.rs:43`), the value the event-scope fixture expects as that
token's read scope (`crates/khive-runtime/src/event_actor_scope.rs:110`–`:115`). An unsplit value decodes by its shape:

- a value with no colon decodes to an `actor` id, as the event read scope treats a bare value
  (`crates/khive-runtime/src/event_actor_scope.rs:54`); the runtime's `kind:id` stamp contains a colon, and so does
  every literal system stamp at this revision (`daemon:checkpoint_task`, `daemon:channel_poll_loop`, `system:propose-apply`;
  `crates/khive-db/src/checkpoint/lifecycle.rs:261`, `crates/khive-mcp/src/serve/channel_poll.rs:567`,
  `crates/khive-pack-kg/src/apply_worker/worker.rs:774`), so only an older bare id has this shape, and the test of
  D3's reservation rule asserts every literal system stamp contains a colon;
- a value whose text before its first colon is a reserved kind decodes to a system identity,
  `LegacyIdentity { kind, exact_id }` split at that colon, under which no credential is admitted;
- any other value decodes to an ambiguous legacy value, `LegacyIdentity::Ambiguous { exact_value }`. The older bare id
  of an `actor` credential whose id contains a colon, such as `svc:build`, and the stamp of a credential of an unstamped
  kind, such as `service:worker`, are the same kind of bytes, and nothing in the row says which wrote it.

An ambiguous value is its own legacy identity, distinct from every pair it might have come from. Its label, the value
itself, is the label either origin is admitted under, and is reserved. It is no reader's own row (D8): inspection
reaches it, and an explicit, evidenced mapping may link it to one identity, whose own view then selects it. The
inventory reports each ambiguous value with its row count and the configured principals whose label equals it. An older
bare id that begins with a reserved kind therefore reads as a system identity, and an older bare id that contains a
colon and begins with no recognized or reserved kind reads as ambiguous; both are reachable by inspection and by no
credential's own view unless mapped, so in these two branches the error runs toward reading less. An older bare id that
begins with a recognized kind does not: it is the first residual below. Enablement closes the open kind set for new admissions: in an enabled store legacy ingress
refuses a credential whose kind is not in `RUNTIME_STAMPED_ACTOR_KINDS` (D5), so every event a legacy credential
admitted after enablement stamps splits. A producer that stamps a credential of a new kind adds that kind to the
constant in the same change, as the constant's documentation asks (`crates/khive-gate/src/actor.rs:5`–`:8`).

Two residuals are not closed, and enablement creates neither: in each, the source's default scope already gives the row
to the pair its bytes decode to (`crates/khive-runtime/src/event_actor_scope.rs:56`–`:62`), and an enabled store's own
event view selects a subset of what that scope selects (D8), so enablement adds no reader to any of these rows.

First, an `actor` id may itself begin with a recognized kind, since a configured id is wrapped whole as the `actor` id
(`crates/khive-runtime/src/actor_identity.rs:20`). The runtime stamps a configured credential `actor:worker` as
`actor:actor:worker`, and the event read scope gives that id no bare alias (`:56`–`:58`), but an older producer that
wrote the id bare wrote `actor:worker`, the same bytes as the stamp of `{actor, worker}`; it decodes by the first rule
above to `{actor, worker}` and is that pair's own row, as it is today. Bare `agent:worker` and `anonymous:local` written
for configured ids of those spellings behave the same way. The bytes do not say which wrote them, but such an id leaves
traces the inventory reads: a configured principal whose `actor` id begins with a recognized kind and a colon, and a
stored value that is the runtime's stamp of such an id (`actor:actor:…`, `actor:agent:…`, `actor:anonymous:…`). The inventory reports each trace with its row count beside the row count of the value it overlaps (`actor:worker` for the id `actor:worker`). Enablement refuses unless the request's list of accepted residuals equals the set of reported traces, the empty set included: a list that omits a reported value refuses, a list that names a value the inventory did not report refuses, and with no trace reported only an empty or absent list is accepted. The inventory record, with the accepted list, is written in the enablement transaction, so an enablement whose record cannot be written does not commit, and `actor.inventory` reads it back. An id that left no trace, written only bare
and no longer configured, is not detectable.

Second, a kind may itself contain a colon, since `ActorRef` checks only that kind and id are
non-empty (`crates/khive-gate/src/actor.rs:57`–`:69`). A credential of kind `actor:custom` and id `worker` stamps
`actor:custom:worker`, which splits as the `actor` id `custom:worker` and is the same bytes as that actor's stamp. No
producer in this repository mints such a kind at `a1a60f3f`: every production `ActorRef` construction names `actor`,
`agent` or `anonymous`, or resolves through `resolve_actor` (`crates/khive-runtime/src/actor_identity.rs:18`). Ingress
refuses such a kind after enablement, as above, so the residual is history that an embedder minting colon-bearing kinds
wrote before enablement, including any event that work it admitted before enablement appends after the inventory (the
bounded exception above). The store cannot detect it; an embedder that has minted such a kind, or holds admitted work under one, does not enable a
store holding that history without an evidenced mapping for it.
A message field stores the credential's id as written, without its kind
(`crates/khive-pack-comm/src/handlers.rs:142`), a task's assignee stores the string its assignment named, and a recall binding's actor field stores the string `brain.bind` was given, `*` when it named none (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:1553`, `:1639`–`:1641`); its consumers compare it to different keys (D8). The label an identity is admitted under is the one `ActorRef::label()`
gives (`crates/khive-gate/src/actor.rs:104`): the id for kind `actor`, the whole `kind:id` for any other kind. So a
credential `svc:build`, or `team:svc:build`, whose only trace is the event stamp `actor:svc:build`, or
`actor:team:svc:build`, reserves `svc:build`, or `team:svc:build`.

A legacy identity is its decoded pair, or an ambiguous value, never its label. Two pairs can share a label: `{agent, worker}`, stamped
`agent:worker`, and `{actor, agent:worker}`, a configured credential `agent:worker` stamped `actor:agent:worker`, both
have the label `agent:worker` (`crates/khive-gate/src/actor.rs:104`–`:108`). They are two identities, as the event read
scope keeps them: an id that splits by the stamped-kind rule gets no bare alias
(`crates/khive-runtime/src/event_actor_scope.rs:50`–`:62`). Their shared label is reserved once and is an unresolved
collision: each pair keeps its own history, and no allocation or mapping claims the label without evidence naming the
pair. No values are coalesced, and a reservation maps nothing.

Enablement is fenced against serving state from before it. The store keeps a mode marker, written in the transaction
that creates the reservation table, and every runtime records the mode it was constructed under. Every transaction a
runtime opens on the store, for an operation or for its own background work (queued effects, workers, checkpoint and
replay tasks), in a store with the registry off as well as an enabled one, reads the marker first and refuses, before
any other read or write, when the marker differs from the runtime's construction mode; the operation or effect it
belongs to fails with it. Admitting an operation, and re-admitting a queued effect (D6), also reads the marker, before
any handler or effect runs and whether or not it later opens a transaction. The read sits in the one dispatch function
that ordinary and intercepted dispatch, batch members, chain steps, aliases and internal calls pass (D7), so an
operation a handler serves from memory, such as `brain.bindings` reading the pack's cached binding state
(`crates/khive-pack-brain/src/handlers/brain_handlers.rs:1762`), refuses with the rest, whether or not an event store is
bound. So once enablement commits, a runtime constructed before it, the tokens it issued, its pending operations and its
long polls cannot read or write under the old rules: the next admission or transaction refuses, a long poll's re-query
included, and the runtime must be constructed again over the enabled store, where D5's construction checks apply. A read
transaction whose snapshot predates the commit sees only the store as it was before enablement. A token issued before
enablement carries no actor binding and is a legacy credential to a runtime constructed after it.

### D4. Create is idempotent; update and tombstone compare revisions

Create takes a tenant-scoped idempotency key and a canonical digest of the request, committed in the same
transaction as the allocation. An exact retry returns the original receipt, even after later edits or a tombstone; a
different request under the same key is a conflict; a handle conflict is never an idempotent success. The receipt
proves allocation, not current liveness.

Update and tombstone carry `expected_revision`, compared in the same writer transaction; a stale revision changes
nothing. The embedder validates a new default project before calling; the runtime records the id it is given and
never calls the embedder. The embedder rechecks the project when an operation uses it. Validation followed by the
project's deletion remains a race this record does not close; no distributed transaction is claimed.

### D5. Dispatch mints a sealed operation context

For registered mode, dispatch constructs a context that callers cannot build themselves. It carries the trusted tenant
scope, the credential principal, an optional human subject, the effective actor (UUID and handle), the resolved
project id, the actor revision read at admission, the credential and grant references needed for revalidation, the
assurance evidence with a non-secret proof reference, and an operation correlation id. Construction happens inside
dispatch, after the trusted host supplies verified credential results and the operation's policy checks pass. Tokens
project the context into nested calls without loss, and `EventAttribution` captures its persisted fields.

Adding optional fields to `RequestIdentity`, which any caller can construct, would let those fields be fabricated and
would keep principal and actor ambiguous. Legacy ingress stays as a named mode. The sealed context has three
constructors, all private to dispatch: a legacy context, built after legacy ingress in every mode, whose principal and
actor are the legacy credential's label; a registered context, built only in an enabled store and only for an actor that
`read_for_admission` (D6) finds live; and the host-administrative context (D1), which carries no effective actor and is
accepted only by management operations. In a store with the registry off, legacy ingress keeps today's behavior exactly.
In an enabled store it keeps today's routing and string serialization with four changes, which the hosted record
requires of an enabled tenant. First, ingress refuses a credential whose kind is not in `RUNTIME_STAMPED_ACTOR_KINDS`
(D3), and a credential whose label equals a registered handle, live or tombstoned, unless an evidenced mapping links
that legacy identity to that actor; the check is a fresh registry read made before any read or write of the operation. When the label parses and is neither registered nor reserved, admission
reserves it in a writer transaction that re-reads the registry first, and refuses if the label was allocated in between
(D3); a runtime that cannot write the store refuses such an admission rather than serve an unreserved label. Second, an
Identity input (D7) that names an actor other than the legacy credential's own label, or a wildcard, refuses, and an
omitted Identity input means that label. Third, an Address input equal to a registered handle names that actor: the row
keeps the string and records the actor's UUID beside it (D8), and a tombstoned handle refuses as an actionable Address.
Fourth, the credential's own reads follow D8's owner rule for legacy readers: message, task and binding owners keep the comparison each consumer makes today, in its field's own encoding and with its own key, and the default scope of an event read or count narrows, so an
`actor` id that contains a colon no longer matches the bare value spelled like it (D3, D8).
So under a legacy credential in an enabled store, `comm.inbox` or `comm.thread` with a `mailbox_actor` naming another
actor refuses even where a configured delegation grants that read with the registry off; `brain.bind` naming another
actor refuses, an omitted binding actor binds the caller rather than `*`, and `brain.unbind` with no actor touches only
the caller's bindings; a message sent to a registered actor reaches that actor's UUID-selected inbox. Because these
rules read the declarations, a runtime over an enabled store refuses at construction any loaded handler without one
(D7), in either mode. A runtime constructed before enablement never serves under these rules; the mode marker (D3)
refuses its operations and transactions instead.

The trusted host's verified credential result names the actor the credential is bound to. Dispatch compares it with
the actor the operation selects and refuses a mismatch; a result that is malformed, partial or of an unknown version
refuses rather than falling back to legacy ingress.

`VerifiedActor` is a host assertion, not an ADR-127 assurance class. ADR-127 names `ActorSignature` and `DaemonBearer`
(`docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:78`); neither may stand for an upstream OAuth check.
This record amends ADR-127 with a host-verified assurance class carrying issuer, method and proof reference, which a
policy may refuse. The embedder verifies OAuth; the runtime never calls it. The new class does not satisfy ADR-127's
requirement of `ActorSignature` for consequential actions (`:86`) and does not relax its boot invariant for protected
verbs (`:214`): a protected verb stays unavailable to a host-verified context unless a later amendment says otherwise.

The project is resolved per operation: an explicit target first, otherwise the actor's current default, then the
embedder's project admission. The context carries the resolved project. A later default change never retargets work
already admitted. Registered mode routes to the admitted project and skips the implicit `local` write and read
expansion at `crates/khive-runtime/src/pack/dispatch.rs:855`; legacy mode keeps it exactly.

### D6. Admission reads the registry fresh, and deferred effects re-admit

`read_for_admission` is a fresh read of the primary registry, never a caller-held reader, a cached snapshot, a
replica, an event projection or an actor object held by a connection. The backend supports cached multi-call read
transactions that keep a WAL snapshot open (`crates/khive-db/src/sql_bridge/sqlite_reader.rs:27`, `:54`), so a
same-store read is not current by construction. The successful read of lifecycle and revision is the admission
point: a tombstone committed before it refuses the operation; one committed after it does not cancel an operation
already admitted. Each member of a batch is admitted on its own. An atomic batch either states its whole-unit
admission order, so that a tombstone written by an earlier member binds later members, or excludes registry
mutations from its atomic surface.

The embedder composes this read with its own grant, entitlement, subject and project checks. There is no atomic
snapshot across the two systems, and this record claims no cross-system linearizability; the embedder documents the
compound admission interval and revalidates a dependency that changed. The hosted embedder's design makes
admission its revocation cut point; this record supplies the runtime half of that cut point, and whether
the composed interval meets it is the embedder's acceptance (activation prerequisite 6). A registry that cannot be
read refuses registered work. It never falls back to a legacy token or label.

Telemetry for an operation already admitted or completed keeps that operation's attribution and may be written after
the actor is tombstoned. A queued new effect is re-admitted with fresh authority supplied by the host that resumes it:
the host re-evaluates the grant, use permission, subject, entitlement and project permission as they stand at resume,
and a captured grant reference is a key for that lookup, never a decision. An actor that is still active does not make
a revoked grant usable. A deferred surface without that host integration stays unavailable to actor-bound requests.

### D7. Actor-valued arguments are declared by type, in the handler schema

Every accepted argument path declares either a non-actor type or an actor-bearing type, and an actor-bearing type is
either **Identity** or **Address**. The declaration also names the role (sender, mailbox owner, binding owner,
recipient, assignee, inspection filter), cardinality, omission behavior, aliases and exact or pattern selection, and
it covers nested objects and arrays and kind-dependent properties, not only top-level names. A handler with no
parameters certifies an empty schema; `params: &[]` alone is not that certification.

The declaration extends the pack-owned metadata that already feeds discovery (`crates/khive-types/src/pack.rs:154`,
`:188`) into a checked contract. It is the one source: a central table is generated from it, never maintained beside
it; names such as `actor` or `assignee` are never used to infer it. Argument decoding is generated or checked against
the same schema, protected shapes refuse unknown fields, and attribution, mailbox, binding and assignment sinks take
typed actor references, so a handler cannot pass an arbitrary string to one. Pack code is trusted linked code; the
registry enforces typed APIs, conformance checks and mutation tests, and does not claim to infer the semantics of
arbitrary Rust or SQL.

In actor-bound mode, Identity inputs (sender and provenance, mailbox owner, recall-binding owner) default to the
effective actor when omitted and refuse a different actor, a wildcard or an unresolved value. Address inputs
(recipient, assignee, permitted inspection filters) resolve without granting anything; eligibility stays a gate and
embedder decision, and an external address is its own typed variant under its channel's policy. A sender filter on
an inbox read is an Address filter even though the parameter is called `from_actor`. Generic create, update, proposal
and ingest paths declare per-kind property semantics, so `properties.from_actor` cannot evade a sender check; a
conflicting submitted identity is refused, never silently replaced.

Actor arguments are resolved at the dispatch seam into a sealed resolved view used by the handler and by the gate.
`GateRequest.args` keeps the submitted arguments for audit. This narrows ADR-129 Amendment 4, which separates
submitted from effective arguments (`docs/adr/ADR-129-fail-closed-gate-default.md:500`), for actor semantics only;
hooks cannot replace a resolved identity or address after admission, and handlers validate domain rules, not
permissions. Ordinary and intercepted dispatch, batch members, chain steps, aliases and internal calls all pass the
same function (`crates/khive-runtime/src/pack/dispatch.rs:291`, `:708`). Building an actor-bound verb registry refuses
any reachable handler or actor-bearing path that has no declaration, and so does building any registry over an
enabled store (D5). Legacy-only packs keep their behavior and cannot be selected into an actor-bound registry or a
runtime over an enabled store.

Local mailbox delegation stays in a store with the registry off: `MailboxReadGate` may grant another actor's view
(`crates/khive-gate/src/mailbox.rs:183`). In an enabled store the mailbox owner is an Identity input under either
credential, so a foreign one refuses (D5 and the actor-bound rule above); delegated reads in an enabled store are a
later decision, not a capability of this record. The delegation code stays for stores with the registry off.

### D8. New registered rows add UUID references; history is not rewritten

Existing string fields stay. A registered write derives both the handle and the UUID from the one sealed reference
and stores both, for example a message's `from_actor_id` and `to_actor_id`, a task's `assignee_id`, and UUID-bearing
binding references. New event rows add the credential principal, optional subject, effective actor UUID and resolved
project as typed, versioned fields. Old `events.actor` bytes are not changed, and old rows are not presented as
carrying all four identities. The two existing actor spellings are not interchangeable: event stamping writes
`kind:id` (`crates/khive-runtime/src/event_store_guard.rs:40`) and comm stores `token.actor().id`
(`crates/khive-pack-comm/src/handlers.rs:142`).

Each actor reference in a row carries its own UUID or none; a row is not classified as a whole. A registered sender
writing to an external recipient stores the sender's UUID beside the recipient's typed external address. A reference
with no UUID is legacy. It may be displayed or joined through an evidenced mapping, and is never promoted to
authenticated attribution by parsing its string. A view selects the rows that are the reader's own by one owner
reference per row: the recipient of an inbound copy, the sender of an outbound copy, whichever of the two a thread
view's participation test matched, the assignee in an assignee-selected task view, the actor of a recall binding, and
the effective actor of an event in an actor's own event view, the default scope of an event read or count such as
`brain.event_counts` (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:298`). An event's credential principal and
subject describe the row. An event inspection the caller is authorized for, an all-actor read by a configured fleet
reader or an explicit actor filter under ADR-103 Amendment 5 (`crates/khive-runtime/src/event_actor_scope.rs:54`), is a
filter, not an owner view, and follows the Address-filter rule below; it names a legacy identity by a typed legacy
reference (D3), never by a string equal to a registered handle. Every path that answers an event query by actor, the
runtime's event store wrapper (`crates/khive-runtime/src/event_store_guard.rs:103`) and the events backend's query
service (`crates/khive-runtime/src/events_split/server.rs:306`) included, takes its actor predicate from this rule as
typed references, never as a label list matched as strings. For messages this is ADR-007's view rule
(`docs/adr/ADR-007-namespace.md:111`, `:115`), kept as it is; this record decides only how an owner reference matches.
Owner selection is by Identity. A registered reader's owner reference matches by UUID, or a legacy reference through an
evidenced mapping; a legacy reference whose string equals a registered handle is not that actor's mail, task or binding
without one. A legacy reader's owner reference matches, among references that carry no UUID, in the encoding its field stores and
with the comparison the source makes today, or through an evidenced mapping that links the reader's legacy identity to
the UUID it carries: a message sender or recipient compares the credential's id
(`crates/khive-pack-comm/src/handlers.rs:142`, `crates/khive-runtime/src/mailbox_view.rs:148`), a task assignee
compares the string as its assignment stored it, a recall binding compares the key each consumer passes today, and an event compares by the event rule below. The binding consumers pass different keys: the binding listing and a `brain.resolve` that names no actor pass the caller's label (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:1758`, `:1768`; `:541`–`:549`), feedback's profile resolution passes the credential's id (`:802`–`:804`, through `ActorRef::binding_id`, `crates/khive-gate/src/actor.rs:94`–`:99`), and the anonymous caller passes none to either resolver, so it reaches only wildcard bindings (`brain_handlers.rs:548`). Each consumer keeps its key in an enabled store, and wildcard and priority resolution are unchanged. For kind `actor` the label and the id are one string; for kind `agent` they differ, so `{agent, worker}` lists and resolves the binding stored under `agent:worker` while its feedback resolves the one stored under `worker`, which a configured credential `worker` also lists and resolves, as today. Because an enabled store lets a legacy credential bind only under its own label (D5), an `agent` credential's own binding is not the one its feedback resolves; aligning those keys changes which profile serves feedback, and this record leaves it to a later decision. A message field holds no kind, so credentials of different kinds with one id, such as the
anonymous caller `{anonymous, local}` and a configured credential `local`, or `{agent, worker}` and a configured
`worker`, share their legacy rows exactly as they do today; the inventory (D3) reports each legacy message owner id that more than one admitted credential holds, and each binding actor string that the keys of more than one admitted credential select, with its row count, and an evidenced mapping may narrow it. The anonymous `local` caller reading its own mailbox, not a delegated one (`crates/khive-pack-comm/src/handlers/validation.rs:261`–`:263`, `crates/khive-pack-comm/src/handlers/inbox.rs:12`), also selects an unattributed pool, and the pool differs by reader; each reader keeps exactly its own. Generic message reads (get, list, search, and any note read that meets a message row) select an inbound row whose recipient is missing or null, an outbound row whose sender is missing or null, and a row with no direction and no text sender or recipient (`crates/khive-runtime/src/mailbox_view.rs:60`–`:83`). `comm.inbox` selects only an inbound row whose recipient is missing or null (`handlers/inbox.rs:143`–`:180`, `crates/khive-db/src/stores/note.rs:1176`–`:1184`), and its sent view only an outbound row whose sender is missing or null (`handlers/inbox.rs:198`–`:205`, `note.rs:1109`–`:1115`). `comm.thread` selects, within the thread, a row whose recipient is missing or null (`handlers/thread.rs:195`–`:205`, `validation.rs:265`–`:268`). The probe selects none: it requires a text recipient and the inbound direction (`crates/khive-pack-comm/sql/probe_messages_select.sql:10`–`:12`, `:31`–`:33`). No other legacy reader and no registered reader inherits a pool, and inspection reaches the pool rows. A pool is the reader's unattributed branch only: a row in one reader's pool whose other field names an owner stays in that owner's views by the ordinary owner rule, exactly as today. An outbound row with sender `local` and no recipient is in the anonymous caller's thread pool and in a configured credential `local`'s generic reads and sent view, and an outbound row with recipient `local` and no sender is in the anonymous caller's generic and sent pools and in that credential's thread view (`handlers/thread.rs:202`–`:204`, `validation.rs:271`–`:276`). In an enabled store a legacy reader's own view therefore selects a subset of the rows the source selects for it today, apart from rows an evidenced mapping links to its legacy identity: enablement adds no legacy reader to any row and removes the ones this rule names, and only a mapping, an explicit act carrying its evidence, adds one. This holds whatever the order of
allocation and admission. Legacy admission reserves the reader's label (D5), so no actor is allocated later under the
label of a legacy reader admitted earlier, a long poll that re-queries included; the owner rule keeps a legacy reader
from a row whose owner reference carries a UUID even where that reference's string equals the reader's label, and
neither the reservation nor the ingress refusal (D5) carries this. The owner rule covers every reader that selects its
own rows by an actor label, including mailbox, thread, probe, task, binding and event reads and the list, search and
count paths over the same rows. In an enabled store, a legacy reader's own event view, among events with no UUID,
selects exactly the stored values that D3 decodes to its own pair, so no value is the own row of two readers: for a
credential of kind `actor`, the stamp `actor:<id>`, and the bare `<id>` as well only when that id contains no colon; for
a credential of kind `agent` or `anonymous`, the stamp `kind:id`. Ingress there admits no other kind (D5). A system
identity and an ambiguous value (D3) are no reader's own row unless an evidenced mapping links them. That is the
source's default scope (`crates/khive-runtime/src/event_actor_scope.rs:56`–`:62`) with one narrowing: an `actor` id
that contains a colon loses the bare alias the source gives it today (`:59`–`:61`; the source's test pins it for
`svc:build`, `:97`–`:102`), so a bare value spelled like that id, which an older producer, a credential of an unstamped
kind or a system stamp may have written, is reached by inspection or an evidenced mapping instead. The narrowing applies
in an enabled store only; with the registry off the source's scope is unchanged. Where configured actor ids contain
colons, as `svc:build` does, an enabled store's own event views drop any older bare rows of those ids until a mapping
links them; the inventory (D3) lists each such value with its row count, so the operator sees the loss
before enabling. Every other reference in a selected row only describes it. An Address filter (D7),
such as a sender filter on an inbox read or a recipient filter on a sent view, only narrows the rows the view already
permits: it matches a registered actor by UUID or through an evidenced mapping and a legacy label among references with
no UUID, and it never admits or drops a row because a reference names someone other than the reader. A view that selects
by project permission rather than by an owner reference, such as a task view under D9, is not narrowed by this rule. A
store with the registry off writes today's shape with no synthetic UUIDs and no new lookups beyond the mode-marker reads
(D3); enabling the registry writes registry, reservation and mapping records and rewrites no old record or event.
Tombstoned UUIDs resolve as historical identities. Using one as an Identity input or as an actionable Address (a new
recipient or assignee) refuses; naming one in an inspection filter over history, such as a sender filter, does not.
Every supplied event id is preserved and retries reuse it (`crates/khive-db/src/stores/event.rs:351`, `:514`); an id is
minted only at original creation, never while forwarding or migrating.

### D9. State is partitioned by the kind of state

| State                                | Registered-mode rule                                                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Registry, lifecycle, default project | Tenant and actor UUID. The default project is an attribute, not identity.                                                                                                                                                                                                                                                                                                                    |
| Inbox, sent and thread messages      | The actor identifies the mailbox; each message's project scopes its data. Reads take the actor and its authorized projects; counts, pagination and cursors filter first. A default change moves no mail.                                                                                                                                                                                     |
| Recall bindings                      | `(actor UUID, project id, consumer)`. Omitted actor means self; omitted project means the resolved project. Ordinary hosted callers cannot create wildcard actor or project bindings (today's omission becomes `*`, `crates/khive-pack-brain/src/handlers/brain_handlers.rs:1553`; legacy mode keeps that in a store with the registry off, and D5's exception applies in an enabled store). |
| Brain profile state                  | Project-scoped; bindings select it per actor, project and consumer. Shared-profile mutation stays administrative. No per-actor cloning, no training pooled across projects.                                                                                                                                                                                                                  |
| Memories and corpus                  | Project-scoped content with actor provenance. Selecting an actor grants no content access. The project is chosen by routing (D5); the namespace stays attribution, so ADR-007's episodic default of the actor's namespace (`docs/adr/ADR-007-namespace.md:491`) is unchanged.                                                                                                                |
| Reference ring                       | `(tenant scope, actor UUID, project id)`, plus a read-scope key so a broad connection's hints do not reach a narrow one (today `(namespace, actor)`, `crates/khive-runtime/src/reference_ring.rs:91`).                                                                                                                                                                                       |
| Recall telemetry and serve ledger    | The full request attribution and the resolved project, never re-resolved through a later default.                                                                                                                                                                                                                                                                                            |
| Tasks                                | The task belongs to a project; the assignee addresses an actor; views apply project permission apart from assignment.                                                                                                                                                                                                                                                                        |
| Session transcripts                  | Project-scoped content; the producer's session identity stays distinct from the actor.                                                                                                                                                                                                                                                                                                       |
| Channel cursors                      | Connector installation state, keyed as today; admission governs delivery destinations.                                                                                                                                                                                                                                                                                                       |
| Daemon maintenance                   | Store scope under an explicit system principal; never a borrowed user actor.                                                                                                                                                                                                                                                                                                                 |

A namespace is not a project id, and namespace visibility is not project permission. Same-tenant store isolation
does not deliver by-ID project limits, because the gate does not see the resolved target
(`docs/adr/ADR-007-namespace.md:277`). Before hosted activation the embedder shows project-isolated routing for every
admitted operation, including cross-project links and batches, or a separately accepted resolved-target admission
design exists.

### D10. Asynchronous work carries an owned context; authority is re-admitted

Spawned work captures an owned, immutable copy of the context at the spawn site and passes it through the closure or
job. Context is never rebuilt from the process environment, never taken from a connection's first operation for
later ones, and never borrowed from the last user by system work. Authority moves only by re-admission (D6), never by
a captured attribution.

The production spawn origins in `khive-runtime` at `a1a60f3f`, and the rule for each:

| Origin (under `crates/khive-runtime/src/`)                                                                     | Rule                                                                                                                                                   |
| -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `audit_batch.rs` supervisor, generation and reaper                                                             | Attribution is captured per event before enqueue; a batch may hold several actors and is never stamped from one context.                               |
| `curation/entity_curation.rs`, `curation/note_merge.rs`, and their writer-task alternatives                    | They capture `EventAttribution` before dispatching work today; they carry the full sealed attribution and the admission proof.                         |
| Embedding tasks in `operations.rs`, `operations/entity_create.rs`, `atomic_message.rs`                         | Carry the expanded token, which cold initialization may use to emit attributed events.                                                                 |
| `runtime/embedder_init.rs`                                                                                     | Keeps the initiating request's attribution; system warmup uses an explicit system context; registered work never gets the configured-runtime fallback. |
| `embedder_registry.rs` embedding thread                                                                        | Writes no attributed row; a future attributable effect adds explicit job context.                                                                      |
| `events_split/events_daemon.rs` child process, `events_split/forwarder.rs`, the events daemon's batch receiver | Envelopes carry per-row attribution and stable ids; transport trust is not request authentication.                                                     |
| `daemon/boot_guard.rs` checkpoint task and warmup                                                              | System attribution (`daemon:checkpoint_task`), never a user's.                                                                                         |
| `daemon/boot_guard.rs` connection processing                                                                   | Context is resolved per logical operation after verified ingress, not once per connection.                                                             |
| `daemon/load_limits.rs` deferred recall work                                                                   | Keeps the request context for telemetry; a distinct protected nested write is re-admitted.                                                             |
| `blob.rs` blob hydration task                                                                                  | Serves a read already admitted for its caller and appends no attributed row; an attributable effect added later carries explicit context.              |

This census covers `khive-runtime` only. Packs, scheduler replays, channels, external `Dispatcher` implementations
and `khive-db` worker internals need their own coverage before hosted activation; the table is not a claim that all
asynchronous work is covered.

## Activation prerequisites

Registered mode can ship in stages, but hosted actor-bound credentials are not issued until all of these hold:

1. The embedder routes each tenant to an isolated store and enables it with its provisioning record (D2).
2. The embedder's session-principal prefixes are in the reservation set (D3).
3. ADR-127 carries the host-verified assurance class (D5).
4. Project-by-ID isolation is shown for every admitted operation (D9), by acceptance item 12 run in the embedder's
   integration harness against its real storage boundary.
5. Every reader and admission path that can receive an actor-bound credential enforces this record. A reader that
   meets an actor-bound version it does not know refuses it rather than reading it as legacy.
6. The embedder documents its compound admission interval (D6) and runs acceptance item 15 against its real policy
   source.
7. Packs, scheduler replays, channels, external `Dispatcher` implementations and `khive-db` worker internals have the
   coverage the D10 census does not give, or stay unavailable to actor-bound requests.

## Phases

Each phase ships only with its own exit items green. Behavior that belongs to a later phase is unavailable until that
phase ships: no actor-bound credential is accepted before phase 4, and a runtime over an enabled store loads only
declared handlers (D7) until phase 5 declares the rest. Stores with the registry off are served as today throughout,
apart from the mode-marker reads (D3).

1. **Mailbox selectors from declared metadata, behavior unchanged.** Add the actor-argument declaration (D7) beside
   handler metadata and declare `comm.inbox`, `comm.thread` and `comm.probe` fully, keeping mailbox-owner and filter
   roles distinct. The gate's selector table (`crates/khive-gate/src/mailbox.rs:48`), the runtime mailbox view's verb
   list (`crates/khive-runtime/src/mailbox_view.rs:110`) and the delegation check's verb list
   (`crates/khive-gate/src/mailbox.rs:188`) all read the declarations, and the three lists are removed. Local and probe
   exceptions, exact label checks, default-deny cross-actor reads and configured delegation stay as they are.
   Declarations appear in discovery, and registry consistency checks cover aliases, duplicate actor paths and missing
   roles. Unconverted verbs stay legacy-only. This phase needs no tenant, session-prefix, pricing or project decision.
   Exit: item 13, and item 1's registry-off rows for the comm verbs.
2. **The canonical parser and the reservation set (D3).** Exit: item 4's grammar, reservation and stamped-kind rows.
3. **Registry persistence, enablement, the management API and enabled-store legacy ingress (D1 to D4; D5's legacy and
   host-administrative constructors and its enabled-store rules; D8's owner rule for legacy readers)**, including the
   storage topology check, the collision inventory, the default designation, the mode marker at admission and in
   transactions, management authorization and the declarations of `comm.send` (sender as Identity, recipient as Address),
   `brain.events`, `brain.event_page` and `brain.event_counts`, so the enabled-store send rows and item 5's own-read and count rows can run. Phase 3 is the first phase in which an allocation and a legacy read meet, so
   the admission reservation and the legacy owner rule ship with it, and the first in which an event appended late can
   meet an allocation, which item 5's late-event row gates. Exit: items 2, 3, 5 and 14, item 4's write, ingress and
   legacy reader rows, and item 1's enabled-store legacy rows for the comm verbs.
4. **The registered context and additive attribution (D5's registered constructor, D6, D8).** Exit: items 6, 7 and 16,
   item 15's admission rows, item 4's registered reader rows, and item 1's registered rows for the comm verbs.
5. **Actor-argument declarations across packs, and the project boundary (D7, D9).** Exit: items 9, 10 and 11, and item
   1's enabled-store legacy and registered rows for the task, recall and brain verbs.
6. **Asynchronous coverage and opt-in embedder integration (D10, activation prerequisites).** Exit: item 8 and item
   15's resume rows. Items 12 and 15 also run in the embedder's harness, as activation prerequisites 4 and 6.

## Acceptance

Each item is a test that fails on a tree without the guard it names. A mutation that removes or bypasses the guard
must turn it red; a compatibility test asserts both the preserved legacy behavior and the new registered behavior,
never only something that already passes on the old tree.

1. With the registry off, representative dispatch, comm, task and recall operations keep today's routing, serialized
   attribution, wildcard defaults and mailbox delegation, including today's `*` binding from `brain.bind` with no actor
   and today's all-actor `brain.unbind`. Under a legacy credential in an enabled store: `comm.inbox` and `comm.thread`
   with a `mailbox_actor` naming another actor refuse, also under a delegation that grants the same read with the
   registry off, an omitted `mailbox_actor` reads the caller's own mailbox, and `comm.probe` naming another actor
   refuses; `brain.bind` naming another actor or `*` refuses, an omitted actor binds the caller, and `brain.unbind` with no actor leaves other actors' bindings. Binding rows, for a credential of kind `agent` and id `worker` with recall bindings seeded with the registry off under `agent:worker` (profile P1) and under `worker` (profile P2): with the registry off and in an enabled store, `brain.bindings` lists only the first, `brain.resolve` naming no actor resolves P1, and feedback's profile resolution resolves P2; with bindings also seeded under `local` (P3), `anonymous:local` (P4) and `*` (P0), the anonymous caller's `brain.resolve` naming no actor and its feedback resolution both resolve P0. A mutant that keys every binding consumer by label fails the feedback row, and one that keys every consumer by id fails the listing and resolve rows. The same fixtures in registered mode take the new path. Removing the mode
   separation, or the enabled-store exception, fails it. No existing row receives an inferred UUID.
2. Concurrent creates of one handle allocate once; an exact retry under the idempotency key returns the original
   receipt after a display edit and after a tombstone; a different request under the key conflicts; a new key cannot
   reuse a tombstoned UUID or handle.
3. Two updates at the same revision produce one winner, and the other gets a stale-revision refusal and changes nothing;
   a tombstone and an update at the same revision resolve by the same rule; an injected failure inside the writer
   transaction leaves no partial actor state and no success receipt. Create cannot set the default designation, update
   cannot move it, tombstoning the default actor refuses, two concurrent enablements leave one bound scope and one
   designated actor, and a store found enabled with no designated actor refuses registered work. A second designated row
   written directly through the storage capability, below the management operations, refuses on the table's constraint.
   Removing the single-designation constraint fails that row, and removing the tombstone guard fails the tombstone row.
   Mode marker: a runtime is constructed over a store with the registry off and with no event store bound, with a
   handler that has no declaration, binding state warmed in memory, an issued token, and a long poll waiting on an
   inbox; enablement then commits through another runtime. The waiting long poll's next re-query refuses, and each later
   operation of the old runtime refuses (the undeclared handler, a delegated mailbox read, `brain.bind` with no actor,
   and `brain.bindings` served from the warmed state) before any read or write other than the marker and before its
   handler runs. A runtime constructed afterwards refuses the undeclared handler at construction and serves the retained
   token as a legacy credential under the enabled-store rules. A mutant that reads the marker only at construction or
   only at an operation's start fails the long-poll row, and one that reads it only inside transactions fails the
   `brain.bindings` row.
4. Grammar and reservations: reserved kinds refuse in every letter case, including lowercase reserved kinds that
   otherwise parse; malformed components refuse; handles exactly at each component maximum and at 162 bytes round-trip,
   and one byte more refuses; legacy and external labels behave as before. A test enumerates every literal kind the
   runtime and packs stamp and fails if one is not reserved. Write and ingress rows: a parseable historical label cannot
   be claimed by a new allocation; a parseable legacy label written after enablement starts and before the inventory
   ends is reserved; after enablement, a legacy credential whose label equals a registered handle is refused at ingress
   before an inbox read, with no row read or written, and is admitted with an evidenced mapping; a legacy send to a
   registered actor keeps its string fields and records the recipient's UUID, and a legacy send to a tombstoned handle
   refuses; a legacy credential whose parseable label appears nowhere in the store is reserved by its first admission, a
   read-only operation included, and a runtime that cannot write the store refuses that admission. Legacy reader rows,
   ordered by barriers: a legacy long poll on a parseable label L that appears nowhere in the store and a legacy list
   and count read over L's mail are admitted and then held; an allocation of handle L then refuses as reserved; a legacy
   sender's message to L stores no recipient UUID, and the released long poll's re-query, the list and the count include
   it. A message row written directly through the storage capability with recipient string L beside a registered actor's
   UUID is not returned to those reads. A legacy sender's sent view, filtered by a recipient naming registered actor R,
   returns its message to R, whose row carries R's UUID. A mutant that does not reserve at legacy admission fails the
   allocation row, one whose legacy readers match owner references by label alone fails the storage-written row, and one
   that applies the owner rule to a filter or to every reference in a row fails the sent-view row. Registered reader
   rows: a registered mailbox read does not return a legacy row whose string equals its handle unless that identity is
   mapped, and does return a legacy-origin message addressed to that actor.
5. Two tenant stores may allocate the same handle; a foreign UUID, a wrong tenant binding and an absent UUID give the
   same public result; enablement without a provisioning record, or with one naming another store or scope, refuses.
   Enablement under a storage configuration with a backend other than the designated store and the events backend
   refuses; with only an events backend beside it, enablement succeeds. Inventory rows: two legacy credentials,
   `svc:build` and `team:svc:build`, whose only trace is the event rows the runtime's audit path wrote for them in the
   events backend, and a label `ops:runner` found only in an event row written directly through the storage capability
   in the older bare shape; after enablement, allocating each of the three labels refuses as reserved, and the two
   credentials are still admitted as legacy. Identity rows: events stamped `agent:worker`, by a token of kind `agent`
   and id `worker` minted as the event-scope tests mint one (`crates/khive-runtime/src/event_actor_scope.rs:76`–`:83`),
   and `actor:agent:worker`, by a configured legacy credential `agent:worker`, inventory as two legacy identities with
   one reserved label; that credential's own event read returns only the `actor:agent:worker` row, and the `agent`
   token's only the `agent:worker` row. An event stamped `daemon:checkpoint_task` by the checkpoint task inventories as
   the system identity `{daemon, checkpoint_task}`: a fleet reader's inspection naming that pair returns it, and the own
   event read of a configured legacy credential `daemon:checkpoint_task` does not. Kind rows: an event stamped
   `service:worker` by a token of kind `service` and id `worker`, minted as the event-scope tests mint one, and a second
   row with the same value written directly through the storage capability in the older bare shape, inventory as one
   ambiguous legacy value with a reserved label and a row count of 2. After enablement a token of kind `service` is
   refused at ingress; a configured legacy credential `service:worker` is admitted, and its own event read returns exactly
   the one `actor:service:worker` row it writes after admission, and its `brain.event_counts` default count is 1; a fleet
   reader's inspection naming the ambiguous value returns both older rows; and the own event read of a configured legacy
   credential `ops:runner` does not return the `ops:runner` row above. A colon-free control: a configured legacy
   credential `runner`, with one event stamped `actor:runner` and one bare `runner` row written in the older shape, reads
   both in its own event read and counts 2. Registry-off rows: in a second store with the registry off holding the same
   two `service:worker` rows, the configured credential `service:worker` and the `service` token each read both, as the
   source's scope does today. Recognized-prefix rows: a configured legacy credential `actor:worker` with one event stamped
   `actor:actor:worker` and one row `actor:worker` written directly through the storage capability in the older bare
   shape; the inventory reports the configured id `actor:worker` and the stamped value `actor:actor:worker` as traces, each with its row count beside the count of `actor:worker`. Enablement naming no residual refuses, and so does enablement naming only `actor:worker`, naming `actor:worker` and the unreported `ops:other`, or naming both traces and `ops:other`; naming exactly the two traces succeeds, and `actor.inventory`, read after enablement and again from a runtime reopened on the store, returns both as accepted residuals. An injected failure of the inventory record's write during that enablement refuses it with no success receipt, and a runtime reopened on the store then finds no mode marker, default designation or reservation from the attempt, and `actor.inventory` reports both traces with no accepted list. After that enablement a configured legacy credential `worker`'s own event read returns the `actor:worker` row as the source's scope does today,
   and the `actor:worker` credential's returns only its `actor:actor:worker` row. In a store whose inventory reports no trace, enablement naming `ops:other` refuses and enablement naming none succeeds. Message rows: in an enabled store the
   anonymous caller's legacy outbound message, stored with sender `local` and no UUID, appears in its own sent view as it
   does with the registry off, and a configured legacy credential `local`'s sent view returns the same row; the
   inventory reports `local` with both holders and the row count; in a store where a credential of kind `agent` and id `worker` and a configured credential `worker` (kind `actor`, whose label and id are both `worker`) are admitted, with recall bindings seeded under `agent:worker` and under `worker`, the inventory reports the binding actor string `worker` with both credentials as holders and its row count, since feedback selects bindings by the agent credential's id while listing and resolve select by label (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:802`–`:804`, `:1758`–`:1768`, `crates/khive-gate/src/actor.rs:94`–`:110`), and does not report `agent:worker`, which only the agent credential's label selects; in a store where `{agent, worker}` and the configured credential `{actor, agent:worker}` are admitted (D3: different ids, one label `agent:worker`), with a recall binding seeded under `agent:worker`, the inventory reports `agent:worker` with both credentials as holders and its row count, since listing and resolve select by label (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:541`–`:549`, `:1758`–`:1768`), and does not report `worker`, which only the first credential's id selects; in a store where the anonymous caller `{anonymous, local}` and a configured credential `{actor, local}` are admitted, with a recall binding seeded under `local`, the inventory does not report `local`, since the anonymous caller passes no actor to either resolver and lists its bindings under `anonymous:local` (`crates/khive-gate/src/actor.rs:94`–`:99`, `brain_handlers.rs:548`, `crates/khive-pack-brain/src/read_scope_tests.rs:737`–`:738`), so only the configured credential selects it, unlike the message row above, where both hold `local`; in a store where `{agent, worker}` and `{agent, agent:worker}` are admitted, with a recall binding seeded under `agent:worker`, the inventory reports `agent:worker` with both credentials as holders and its row count, since the first selects it by label in listing and resolve and the second by id in feedback (`brain_handlers.rs:541`–`:549`, `:802`–`:804`, `:1758`–`:1768`); pool rows, each the same with the registry off and in the enabled store: a legacy message with no recipient, sender or direction is returned by the anonymous caller's generic message list and by neither its `comm.inbox` nor its sent view; an inbound message whose recipient is null and that names no sender, the shape of the existing test `i2166_null_recipient_pool_is_anonymous_only` (`crates/khive-pack-comm/tests/integration.rs:12137`), is in the anonymous caller's `comm.inbox`, its unread count and its `comm.thread` opened on that message as root, and not in its probe; an outbound message with no sender and no recipient is in the anonymous caller's sent view; and none of the three is in any own view of a configured legacy credential `local` or of a registered actor. Overlap rows, each the same with the registry off and in the enabled store and each message opened as its own thread root: an outbound message with sender `local` and no recipient is in the anonymous caller's `comm.thread` and in the configured credential `local`'s generic message list and sent view; an outbound message with recipient `local` and no sender is in the anonymous caller's generic message list and sent view and in that credential's `comm.thread`. Each own
   read and count in these rows covers a window that closes before the first of them runs, so their own audit events
   stay outside it. The own event reads are `brain.event_page` with `since` and `until`, and the counts are `brain.event_counts` over the same window, neither naming an actor nor asking for all actors (`crates/khive-pack-brain/src/handlers/event_page.rs:105`–`:131`, `brain_handlers.rs:277`–`:303`). Late event, ordered by barriers: a legacy credential `lab:late`, found
   nowhere in the store or its history, is admitted before enablement for a read-only operation whose audit event a
   test-support event sink holds before it reaches the events backend; enablement commits, its inventory finishes and
   handle `lab:late` is allocated; the event is then released. It keeps its id and its stamp `actor:lab:late`, carries
   no UUID, and a configured fleet reader's all-actor read returns it marked legacy. Removing the topology check fails
   the first row; an inventory that reserves stored values as written fails the `svc:build` and `team:svc:build` rows,
   one that strips a leading component from every value fails the `ops:runner` row, one that infers a UUID for the
   late event fails its row, one that makes two pairs with one label one identity fails the identity rows, one that
   decodes every unsplit value as an `actor` id fails the `daemon:checkpoint_task` row, one that decodes an unsplit
   value containing a colon as an `actor` id, or keeps the bare alias for an `actor` id containing a colon, fails the
   `service:worker` and `ops:runner` own-read rows, one that drops the bare alias of a colon-free id fails the `runner`
   control, one that admits an unstamped kind at ingress fails the refusal row, one that applies the narrowing with
   the registry off fails the registry-off rows, an inventory that does not read recognized-prefix traces fails the enablement refusals of the recognized-prefix rows, an inventory that leaves out binding actor strings, or computes every credential's binding keys by label alone, fails the shared-binding row, one that reports a binding string only one credential selects fails that row's `agent:worker` arm, one that computes every credential's binding keys by id alone fails the shared-label row, one that gives every credential both its label and its id as binding keys fails the anonymous binding row, one that counts holders of a label and holders of an id separately fails the cross-key row, a guard that refuses only an empty list of accepted residuals fails the partial and unreported-value refusals, one that does not keep the accepted list fails the read-back, one that writes the inventory record in its own transaction after the enablement commits fails the injected-failure row, one that compares the list only while a trace is reported fails the no-trace row, one that matches message owners by label fails the anonymous sent-view row, one that gives `comm.inbox` the generic reads' pool fails the unrouted row, one that drops the inbox pool fails the null-recipient row, one that gives the probe a pool fails that row's probe arm, one that lets a named or registered reader inherit any pool fails the last pool arm, one that takes a pool row out of ordinary owner matching fails the overlap rows, and one that drops the thread pool fails the null-recipient row's thread arm.
6. An actor tombstoned before the admission read refuses, on an existing connection and between two batch members; a
   read snapshot opened earlier cannot make `read_for_admission` accept it; a tombstone after admission lets the
   admitted operation finish; an unreadable registry refuses.
7. One principal acting as two actors in two projects yields the same principal, distinct effective actor UUIDs and
   exact resolved projects in new audit rows; an omitted target uses the current default only when authorized; an
   invalid default fails without falling back to `local`; old rows stay byte-identical. A credential the host result
   binds to one actor cannot select the other, and a malformed, partial or unknown-version host result refuses.
8. Two concurrent operations keep every identity field through nested dispatch, the merge writer queue, cold
   initialization, deferred recall and mixed-actor event forwarding; retries keep event ids; checkpoint and warmup
   never borrow a user's context; a tombstone before a queued new effect refuses it, and reporting a completed effect
   keeps its original attribution.
9. Each mismatch between a handler and its declaration fails at the stage named: an actor argument renamed in the
   handler's decoder but not in its declaration fails the decoder conformance check when the registry is built; an actor
   field nested in an object or array the declaration does not cover fails registry construction; a deleted role fails
   registry construction; an alias or internal handler added without a declaration fails registry construction; a
   resolved value changed after admission fails the sealed-view check at dispatch. A census test enumerates every string
   literal naming a declared actor field in raw SQL and untyped JSON access across the workspace (today
   `crates/khive-runtime/src/mailbox_view.rs` reads `to_actor` that way) and compares the set with a reviewed list; an
   unlisted read fails it. The census sees string literals only: a field reached through a computed name or bulk
   deserialization is outside it, and this record claims no coverage there. Beyond those: a foreign Identity refuses; an
   eligible foreign assignee succeeds; inbox sender filters remain filters; generic create, property and proposal paths
   cannot forge a sender UUID.
10. A local delegated mailbox read still works; the same foreign mailbox read under an actor-bound context refuses;
    an omitted hosted binding actor means self; wildcard bindings and shared-profile lifecycle need administrative
    authority.
11. The same actor in two projects has separate bindings, rings, training and filtered mail, counts and cursors; a
    default change moves no row; a narrow connection cannot recover data or hints populated through a broad one. A
    reader with permission on a project sees that project's task assigned to another registered actor, and a mutant that
    applies D8's owner rule to that view fails it.
12. By-ID reads and mutations, and multi-project links and batches, refuse outside the admitted project authority.
    This item runs in the embedder's integration harness against its real storage boundary (activation prerequisite
    4); a namespace-only mock does not count, and the runtime repository's gates do not claim it.
13. A test-support pack declares a mailbox-owner field under a name none of the removed lists used. The gate's selector,
    the runtime mailbox view and the delegation check all select by it, and the existing comm verbs keep today's
    decisions, local and probe exceptions and delegation. A path that still reads a fixed list misses the renamed field
    and fails it, and so does deleting the declaration. Discovery lists each declared actor path with its role. A
    declaration set with a duplicate actor path, an alias equal to another path, or an actor-bearing path with no role
    fails registry construction, and removing each of those checks fails its row.
14. Under a gate that denies the operation for the asking principal, each management operation refuses before any
    registry read or write; a member context cannot list or tombstone; a registered-mode composition built with
    `AllowAllGate` fails at construction; the host-administrative context can enable and create. Removing the gate call
    from any one operation fails it.
15. With the actor still active, a test-support host adapter whose policy source the test controls revokes the grant,
    removes the use or project permission, suspends the subject, or ends the entitlement. Each change committed before
    admission refuses the operation; each change committed between a queued effect's capture and its resume refuses
    the resumed effect; a completed effect's telemetry keeps its attribution. A mutant that re-reads the registry but
    reuses the captured grant decision fails it.
16. Mixed and historical references: a registered sender's message to an `email:` address stores the sender's UUID
    beside the typed external address and keeps the address string as today; a registered sender's message to a legacy
    label that is not a registered handle stores the sender's UUID and no recipient UUID, and the legacy reader with
    that label receives it in its inbox, list and count. Registered actor B's inbox read with a sender filter naming
    registered actor A returns A's messages to B, and so does a legacy reader's sender filter naming A. After A is
    tombstoned, resolving its UUID and its handle returns the historical identity marked tombstoned, and B's sender
    filter naming A still returns the messages A sent B; naming A as an Identity input or as a new recipient refuses. A
    mutant that refuses a mixed reference, that applies the owner rule to a filter or to a reference other than the
    owner, or that drops tombstoned actors from resolution or from history filters, fails it. The late event of item 5,
    once a registered credential is issued for actor `lab:late`: that actor's own event read and its
    `brain.event_counts` default count exclude it, and the fleet reader's all-actor read still returns it marked legacy.
    Owned rows: the principal of item 7, acting as registered actors X and Y, both permitted on one project `shared`,
    writes two audited operations as X and one as Y, all three admitted to `shared`; X's own event read over `shared`
    returns exactly X's two events and X's `brain.event_counts` default count there is 2, and Y's read and count give its
    one event, every one of these events carrying the same credential principal and its own effective actor. The three
    events share both the principal and the project, so a selector by principal returns all three to each reader and a
    count of 3, with or without a project predicate. The reads and counts cover a window that opens after enablement and
    allocation and closes before the first of them runs, so neither setup nor the reads' own audit events fall inside
    it. A mutant whose event reads or counts match an owner by label string
    fails the own-view rows, one that selects by credential principal, with or without a project predicate, fails the
    owned rows, one that returns no rows or a zero count for a registered
    actor fails the owned rows, and one that drops events with no UUID from inspection fails the inspection row.

## Alternatives considered

| Alternative                                                               | Why not                                                                                                                                    |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Actors as graph notes or entities written through the generic graph verbs | Curation and caller properties would become allocation and lifecycle authority.                                                            |
| Reuse ADR-053's `ActorStore` as CRUD                                      | It is the token-to-caller authentication seam, not a registry, and has no implementation.                                                  |
| A separate actors backend now                                             | Another availability boundary and restore order with no measured need; a later backend must implement the same API and freshness contract. |
| A writable registry in the embedder beside the runtime's                  | Two authorities for one identity, kept in step by a synchronization protocol.                                                              |
| A tenant column on actor rows as whole-store isolation                    | Isolates actor lookups only; entities, messages, tasks and by-ID operations stay shared.                                                   |
| Runtime proof of single tenancy                                           | The schema has no tenant discriminator; a marker is not retroactive evidence.                                                              |
| Tighten `ActorRef` globally                                               | Breaks historical and system labels.                                                                                                       |
| Normalize old labels, or treat parseable ones as registered               | Merges identities and turns history into claims on future allocations.                                                                     |
| Optional fields on `RequestIdentity` as the authenticated context         | Fabricable by any caller; keeps principal and actor ambiguous.                                                                             |
| Task-local or global current actor                                        | Unsafe across concurrent operations and spawned work.                                                                                      |
| Infer actor arguments from names, or keep a hand-written table            | Misses renamed and nested fields; a table drifts from mounted and optional packs.                                                          |
| Handles only as references                                                | Repeats label resolution and cannot tell a registered identity from a historical label.                                                    |
| Rewrite history to UUIDs                                                  | Mutates recorded history and invents identity where the evidence is ambiguous.                                                             |
| Partition everything by actor, or everything by actor and project         | The first leaks project content and learned state; the second duplicates shared project data and system state.                             |

## Consequences

- The runtime gains one identity authority per tenant store and a dispatch context that keeps principal, subject, actor
  and project apart. Stores that never enable the registry run as before, apart from one mode-marker read per operation
  and one per transaction (D3). In an enabled store, legacy credentials keep their routing and string serialization, but
  a credential whose label is an unmapped registered handle is refused at ingress, an Identity input cannot name another
  actor or a wildcard, an Address equal to a registered handle records that actor's UUID (D5), and only declared
  handlers load (D7).
- Actor-argument declarations become part of every pack's schema. This is the largest cross-pack cost, and phase 1 pays
  a small part of it against a consumer that exists today.
- Readers handle two row shapes for actor references: legacy strings and UUID-bearing registered rows.
- Admission costs one fresh registry read per operation in registered mode, and so does legacy ingress in an enabled
  store, which also reserves a parseable label with one write the first time that label is admitted. Enablement is
  available only to a tenant whose actor-valued rows live in the designated store, with events in it or in a separate
  events backend (D2).

## Not decided here

Plans and pricing, consent screen layout, the token and grant schema, isolation for arbitrary shared-store deployments,
the full ADR-127 signature and grant implementation, the ADR-129 global default flip, shared-profile ownership, a
registry cache for display lookups (admission never reads one, D6), moving the default designation, delegated mailbox
reads in an enabled store, enablement for a tenant whose actor-valued rows span several stores, and linearizable
admission across the runtime and the embedder.
