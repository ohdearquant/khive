# ADR-105: Cross-node comm transport (node channel adapter + hub ingress)

- Status: Accepted (signed 2026-07-08, with riders R1 and R2 below)
- Date: 2026-07-08
- Depends on: [ADR-056](ADR-056-channel-transport-layer.md) (channel transport
  abstraction), [ADR-057](ADR-057-comm-actor-addressed-delivery.md) (actor-addressed
  dual-write), [ADR-017](ADR-017-pack-standard.md) (pack vocabulary additivity)

## Context

A khive deployment today is one daemon over one database: every actor whose messages
matter reads and writes the same store, and channels (email, and the planned telegram
adapter) are the only boundary crossers. Distributed pack placement changes that: packs
of one logical organization run on different hosts, security-partitioned, with comm as
the bus between them.

The concrete driver is a three-node topology:

- A **hub node**: a small always-on cloud machine running the comm and schedule packs.
  It is the only node designed to be internet-reachable. It must never hold mailbox
  credentials.
- A **mail spoke**: a residential machine running the email channel, with mailbox
  credentials held locally. It sits behind NAT and must initiate connections only —
  it never listens.
- A **primary spoke**: the residential workstation whose database holds the
  organization's actor inboxes. Same posture: NAT'd, portless, dial-out only.

Two of these constraints are fixed, not design variables: mailbox credentials never
leave the mail spoke, and residential spokes never accept inbound connections. Two
NAT'd, dial-out-only nodes cannot rendezvous with each other directly — a listener both
can dial is a topological necessity, and the hub is the only node permitted to listen.
The star shape is therefore a consequence of the constraints, not a preference.

The success criterion is one end-to-end flow: a message arrives at the mail spoke's
mailbox addressed to an actor homed on the primary spoke, and a `comm.inbox` call as
that actor on the primary spoke returns it exactly once.

## Decision

### Topology: star, hub listens, spokes dial out

The hub exposes one minimal authenticated HTTPS ingress. Spokes connect outbound only.

```text
mail spoke (email pack, creds local)      primary spoke (actor inboxes)
      │  node channel adapter                   │  node channel adapter
      │  outbound HTTPS only                    │  outbound HTTPS only
      ▼                                         ▼
   ┌────────────────────────────────────────────────┐
   │  hub daemon                                     │
   │    POST /node/ingest  → auth → comm.ingest      │
   │    GET  /node/pull    → undelivered for node    │
   │  store-and-forward over the comm note store     │
   │  no mailbox credentials, ever                   │
   └────────────────────────────────────────────────┘
```

### Spoke side: a Channel adapter

A new sibling crate `khive-channel-node` implements the ADR-056 `Channel` trait,
feature-gated as `channel-node`:

- `send(envelope)` → `POST /node/ingest` to the hub.
- `poll(since)` → `GET /node/pull` for this node's undelivered messages.

The existing daemon-role `channel_poll_loop` and `channel_outbox_loop` drive it
unchanged — the node adapter is one more `(kind, slug)` registration in the
`ChannelRegistry`. Everything the email adapter gets from that machinery (external-id
dedup, at-least-once delivery marking, health heartbeats, namespace-aligned ingest) the
node adapter inherits for free.

### Hub side: an ingress module, not a Channel

The hub is the server; the `poll`/`send` abstraction is the spoke's, and it does not
fit the server role. The hub gains a small feature-gated HTTP module with exactly two
handlers over the comm note store it already runs:

- `POST /node/ingest`: authenticate the node bearer **before parsing the body**, then
  call the existing `comm.ingest` subhandler.
- `GET /node/pull?node=<id>&since=<ts>`: list outbound undelivered messages routed to
  that node (the same query shape the email outbox loop runs, keyed by destination node
  instead of an address prefix), stamping `delivered_at` on acknowledgement.

### Addressing and routing: transparent to senders

`comm.send(to="lambda:example")` is unchanged. The node hop is resolved in the
transport layer — the outbox loop — exactly where the `email:` prefix decision already
lives. A static per-node TOML `[node_routes]` table maps actor labels (exact or prefix)
to an owning node id, plus the hub base URL and the name of the environment variable
holding this node's bearer token. The committed repository carries only a generic
example table, never a deployment roster.

`khive:` is reserved as a new channel-kind via pack vocabulary (ADR-017, additive
only): the transport-layer address form is `khive:<node>:<actor>`. Senders do not type
it in v0; reserving it leaves room for explicit node addressing later without a schema
change.

Rejected: a hub-resident dynamic registry (premature for a handful of nodes; a network
round-trip per route and a new consistency authority — adopt only when node count
outgrows a hand-maintained table), and explicit node addressing forced on senders
(leaks topology into every call site).

### Delivery semantics: at-least-once + idempotent ingest

The node envelope's `external_id` is `khive:<origin-node>:<message-full-uuid>`. It
flows through the existing partial-unique dedup index, so a re-pushed or re-pulled
message lands zero rows — the identical guard the email adapter depends on.
At-least-once transport plus idempotent ingest yields effectively-once landing.

Ordering and exactly-once are explicitly out of scope: comm is not order-sensitive
(messages carry `sent_at`, threads resolve by `thread_id`, inboxes sort by
`created_at`). A sequencing/ack protocol would be complexity with no consumer.

### Auth: per-node bearer over TLS

Each spoke holds one bearer token — a platform secret on the hub side, a local
environment variable on the spoke side, never in any khive store. The hub validates the
bearer on every request before touching the body. The platform terminates TLS. Replay
of a message envelope is defeated by external-id dedup; eavesdropping is defeated by
TLS. Mutual TLS is the documented upgrade path if node count or threat sensitivity
rises; it is not warranted for a three-node fleet.

Attribution: a node-relayed message keeps its origin `from_actor`. Re-attributing
relayed messages to the hub's own actor would destroy the sender attribution the email
adapter's hardening exists to earn.

### Scope: comm messages only

The node channel transports `message` notes and nothing else. Memory and KG records are
mutable and conflict-prone; syncing them requires versioning and merge semantics that a
message channel neither has nor should grow. Any state federation is a separate ADR
with its own conflict model. (The existing KG versioning snapshot machinery was
assessed and is wrong-shaped for message latency; it remains the tool for its own job.)

### v0 includes the primary spoke

A hub-and-mail-spoke deployment moves messages to a node nobody reads — it validates
plumbing while delivering nothing. The primary spoke runs the same adapter in the same
portless posture; including it is one more config and one more bearer. v0 is the
three-node star, verified by the end-to-end flow above.

## Sign-off riders (binding)

- **R1 — executable success criterion.** The end-to-end success criterion in Context is
  delivered as an executable smoke script in the implementation lane, not prose. The
  script must include a deliberate re-push of an already-delivered envelope and assert
  exactly-once landing (external-id dedup observed, zero duplicate inbox rows).
- **R2 — auth-before-parse stays testable.** The hub ingress must have a test proving
  that a request with a bad bearer and a malformed body is rejected with zero parse
  attempts of the body.
- The transitive-trust residual risk (Consequences below) is acknowledged for v0 at
  sign-off; its revisit trigger converts to a tracked issue when the first
  implementation PR opens.

## Consequences

- The hub gains its first inbound surface: two authenticated endpoints. This is scoped
  and justified — the hub is the only internet-designed node, the surface is minimal,
  and a hub compromise leaks comm routing but never mailbox credentials.
- The hub is a single point of rendezvous. If it is down, spokes queue outbound locally
  (undelivered notes simply retry, which is existing outbox behavior) and inbound waits.
  Acceptable for v0; matches the existing single-broker channels.
- **Residual risk (explicitly accepted for v0, flagged for maintainer acknowledgement):
  possession of a valid spoke bearer allows injecting messages with any asserted
  `from_actor` fleet-wide,** because origin attribution is trusted transitively across
  the deployment's own tokened nodes. v0 mitigations: token secrecy, the minimal
  single-purpose endpoint, and all nodes being operated by the same organization. Open
  question, revisit if a spoke is ever less trusted than the hub: should the hub
  re-derive trust tier on relay rather than trusting origin attribution transitively?
- Scaling is linear and boring: each added node is a routing-table row and a bearer.

## Implementation fences

- MAY add `khive-channel-node` (Channel impl), the hub ingress module, the
  `[node_routes]` config section, one remote-routing branch in `channel_outbox_loop`,
  and the `khive:` channel-kind (additive vocabulary).
- MAY NOT modify `comm.send`, `comm.reply`, or the dual-write path — remote routing
  lives in the transport/outbox layer only; existing email channel behavior is a
  regression surface and stays byte-identical.
- MAY NOT add any inbound listener or port on a residential spoke.
- MAY NOT place bearer tokens or mailbox credentials in any khive store.
- MAY NOT route this traffic through the commercial cloud deployment: fleet transport
  and product deploy stay decoupled — coupling them makes the product a hard runtime
  dependency of the operator's own communications and entangles fleet changes with
  product release gates.
- MAY NOT transport anything beyond `message` notes.

## Alternatives considered

1. **Infra-level transport (overlay VPN / persistent SSH tunnel) instead of a Channel
   adapter.** Rejected. The cheap part of this problem is moving bytes; the expensive
   part is delivery semantics (normalized envelope, idempotent ingest into the right
   inbox, dedup, delivery marking, health), which the Channel seam already ships. An
   overlay network provides an IP and none of that — every line of forwarding logic
   would still be written, plus a second always-on daemon per node, an external
   coordination dependency, and (for overlay peers) an inbound-accepting posture on the
   spokes that the fixed constraints forbid. A narrow future exception is noted: if a
   hub ever needs a spoke's full daemon surface for state federation, an overlay is a
   candidate transport for that separate design.
2. **Relay through the commercial cloud deployment.** Rejected for coupling: a product
   outage, deploy, or launch hold would take fleet comm down with it, fleet-internal
   traffic would live inside a multi-tenant product database, and an internal
   infrastructure need would entangle with product release gates.
3. **Generic "grow a listener on the hub" without the spoke-initiated discipline.**
   Subsumed: since residential spokes must never listen, the only viable shape is the
   one specified here; this ADR is that shape written down.
4. **KG-versioning snapshot push/pull as the comm transport.** Rejected: built for
   version-controlled graph state, wrong latency and granularity shape for messaging.

## Amendment (2026-07-20): transport layer ships as a commercially licensed extension

The ADR-056 channel-transport layer this design builds on moves out of the
open-source repository as a commercially licensed extension (ADR-056 amendment of the
same date; the extraction lands as a separate pull request). This ADR's design is
unchanged; its MAY-add implementation surface (`khive-channel-node`, the hub ingress
module) lands alongside the extension's trait crate rather than in the open-source
tree.
