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
- MAY NOT route this traffic through any separately-hosted service deployment: node
  transport and any hosted service stay decoupled — coupling them makes the hosted
  service a hard runtime dependency of the operator's own communications and
  entangles node-transport changes with an unrelated release cadence.
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
2. **Relay through a separately-hosted service deployment.** Rejected for coupling: an
   outage, deploy, or launch hold on that service would take node-to-node
   communication down with it, node-internal traffic would live inside a
   multi-tenant database it does not need, and an internal infrastructure need would
   entangle with an unrelated release cadence.
3. **Generic "grow a listener on the hub" without the spoke-initiated discipline.**
   Subsumed: since residential spokes must never listen, the only viable shape is the
   one specified here; this ADR is that shape written down.
4. **KG-versioning snapshot push/pull as the comm transport.** Rejected: built for
   version-controlled graph state, wrong latency and granularity shape for messaging.

## Amendment 2026-09-14 -- Hosted messaging profile: directory, consent, client custody

**Instruction.** Product ruling, 2026-09-14: khive ships an agent-to-agent messaging
product: any khive agent can contact any other once both are in each
other's contacts; what a client sends stays on that client, the service keeps only server logs;
the product is built in the hosted service, and any runtime interface it needs is made here
under this document's own process. This amendment records what that ruling changes in ADR-105
and what it leaves standing. The product contract itself is C-ADR-033 in the hosted service's
decision record; this amendment is the runtime half.

### The fifth fence is overridden for the opt-in hosted profile

ADR-105's fifth implementation fence reads, verbatim:

> MAY NOT route this traffic through any separately-hosted service deployment: node
> transport and any hosted service stay decoupled — coupling them makes the hosted
> service a hard runtime dependency of the operator's own communications and
> entangles node-transport changes with an unrelated release cadence.

The 2026-09-14 product ruling overrides this fence for one profile only: a spoke that opts in
to the hosted messaging profile dials the hosted service as its hub. The fence's two reasons
were true and remain true, so the profile carries their costs explicitly: the hosted service
becomes a runtime dependency of that spoke's cross-deployment messaging (never of its local
comm, which is unchanged), and the node protocol is versioned so that the two release cadences
meet at a version number rather than a shared build. A spoke that does not opt in is exactly
the ADR-105 spoke and the fence stands for it.

### What changes

1. **Directory instead of `[node_routes]`.** In the hosted profile the static routing table is
   replaced by the service's directory: a stable, service-assigned agent identifier per
   participant, separate from the node name, the local actor id and the device credential.
   Routing is by canonical address, resolved by the hub against consent state on every send
   and retry. A spoke keeps `[node_routes]` for the self-hosted hub of the original design.
2. **Consent instead of a roster.** ADR-105's trust is transitive across one organisation's
   nodes. The hosted profile has many organisations that do not trust each other, so a hop is
   authorized only by an active, bilateral, revocable contact grant between the exact pair of
   agents, at a generation the hub checks at ingress and again before forwarding. Revocation is
   observed at the next send; it does not retract a forward already authorized.
3. **Ingress-derived attribution.** `from` is never read from the wire. The hub derives the
   sender from the authenticated device credential and its live device-to-agent binding, and
   the spoke's trusted ingest fixes the recipient from local transport authority. An asserted
   `from`, tenant, namespace, actor or project field in an envelope is refused, not corrected.
4. **Client custody.** The hub forwards ciphertext from a bounded memory buffer while both
   clients are connected and writes no body, subject or plaintext-derived digest anywhere. The
   sender's outbox and the recipient's store are the only durable copies. "Offline delivery"
   in the ADR-105 sense (hub queues while a spoke is down) does not exist in this profile; the
   sender stays `pending` until the recipient signs for the message.
5. **Receipts.** At-least-once over idempotent ingest stands, and gains a durable, signed
   recipient receipt so that "delivered" means the recipient committed the note. The `Channel`
   trait (ADR-056) gains three defaulted methods and their companion types, so every existing
   adapter keeps its behaviour with no source change:
   - `send_with_receipt(envelope) -> SendOutcome`; default calls `send` and answers
     `LegacyAccepted`. The node adapter answers `Pending`, `RecipientStored(receipt)` or
     `RecipientQuarantined(receipt)`. The node outbox refuses `LegacyAccepted` as proof.
   - `poll_deliveries(since, checkpoint) -> DeliveryPage`; default wraps `poll_page` with no
     receipt tickets. A node page pairs each envelope with a typed, non-transferable
     `InboundReceiptTicket` (authenticated routing identity, key and contact generations,
     logical message identifier, delivery-attempt identifier); the runtime rejects a
     mismatched or duplicated ticket.
   - `acknowledge_receipt(receipt) -> Result<(), ChannelError>`; default unsupported,
     mandatory for the node adapter, driven by a durable acknowledgement journal that the
     node loop retries after restart. Polling never acknowledges or deletes on its own.
     The receipt is a signed tuple: protocol version, logical message identifier, sender and
     recipient agent identifiers, recipient device and key epoch, contact generation,
     delivery-attempt identifier, disposition `stored` or `quarantined`. It carries no subject,
     body, local note identifier or free text.
6. **Trusted node ingest.** `comm.ingest` gains a verified-recipient mode that can only be
   entered from the daemon's own node loop holding a valid ticket, never from wire
   parameters. It fixes the recipient, confines correlation to the authenticated
   conversation, and commits the message note and the transport receipt record in one
   transaction, so a duplicate delivery lands zero rows and a failed write yields no receipt.
   The transport record survives explicit history deletion. Email recipient selection outside
   this mode is unchanged.
7. **Runtime-owned transport state.** Node outbox rows, the acknowledgement journal, replay
   identity and the receipt record are runtime-owned; no adapter writes raw SQL. A
   narrowly scoped transport-status operation reports `pending`, `recipient_stored`,
   `recipient_quarantined`, `failed` or `unknown`. `comm.delivered` keeps its existing
   meaning (the internal dual-write question) and is not the transport status.
8. **Node loop lifecycle.** The node receive, outbox and receipt-retry tasks are independently
   cancellable, with per-credential health and bounded backoff. An authentication failure
   pauses the affected channel for credential repair rather than discarding pending messages,
   as the email loop already does. The email tasks are untouched.
9. **Vocabulary.** The `khive:` channel kind stands. Added: the versioned product address
   form, `logical message identifier`, `device grant`, `contact generation`,
   `recipient key epoch`, `pending`, `recipient_stored`, `recipient_quarantined`, `unknown`,
   `LegacyAccepted`.

### What stands

Every other fence, verbatim in force: `comm.send`, `comm.reply` and the dual-write path are
not modified, remote routing lives in the transport and outbox layer only, and existing email
channel behaviour is a regression surface that stays byte-identical; no inbound listener or
port on a spoke; no bearer tokens or mailbox credentials in any khive store; nothing beyond
`message` notes is transported. Riders R1 and R2 stand and extend to the hosted profile: the
end-to-end success criterion is executable (two independently owned spokes, one message each
way, exactly one recipient note per logical identifier, local history readable with the hub
stopped), and the hub authenticates before it parses.

The MAY list is extended, not reinterpreted: MAY add the three defaulted `Channel` methods and
their companion types, the verified-recipient ingest mode, the runtime-owned transport state
and the transport-status operation, the node loop tasks, and the vocabulary above.

### Compatibility obligation

Every existing channel adapter is in the compatibility matrix even where the defaulted methods
mean its source does not change: serialized messages, routing, retry classification, cursor
behaviour and send effects are asserted byte-identical before and after this amendment. The
registry selects an adapter by exact `(kind, slug)`; a kind-only lookup that returns an
unspecified member where several exist is not used on any node path.

<!-- deno-fmt-ignore-start -->

## Appendix A (2026-09-23) -- Node wire protocol, version 1

**Status.** Proposed under this document's process, as part of the 2026-09-14 amendment. It is
normative for the hosted profile and for nothing else. The test vectors in A.11 are part of the
specification: an implementation that disagrees with them is wrong, whichever side it is on.

This appendix fixes the bytes that the node adapter (item 5 of the amendment, `khive-channel-node`)
and the hosted service's node ingress exchange. It exists so that both can be built against one
contract at the same time, and so that neither side's first implementation becomes the definition.

### A.1 Scope and versioning

In scope: device request authentication, contact key lookup, envelope submission, delivery polling,
recipient receipts, and sender-side status. Out of scope: the owner's directory operations
(enrolment as an owner action, share cards, contact requests, acceptance, blocks, revocation), which
live on the hosted service's owner-authenticated surface. The only parts of enrolment defined here are
the key bundle the client generates, because the client must produce it, and three rules replay
identity depends on: replacing either key starts a new epoch, a device's epochs only increase, and a
device identifier is never reassigned (A.3).

Transport is HTTPS. Every connection originates at the spoke; no spoke listens. Bodies are UTF-8
JSON. Every path is under `/node/v1/`. Protocol version 1 fixes the cipher suite of A.5; a different
suite, a different signature scheme or a different byte layout in A.2 to A.5 is a different protocol
version with a different path prefix. A version 1 server refuses any other `protocol_version` value
with `unsupported_version`.

### A.2 Encodings

- **Identifiers.** Agent, device, logical message and delivery attempt identifiers are UUIDs. In JSON and
  in request headers they are written in canonical lowercase hyphenated form; in any signed or hashed input they are the
  16 raw bytes. In a request body or a path parameter, a value that is not a UUID, and a
  non-canonical spelling of one (uppercase, braces, no hyphens), are `invalid_request`; a spelling is
  never normalized. An implementation may hold these
  identifiers as text internally, provided it parses them to the 16-byte form at this boundary.
- **Address.** `khive1:<realm>/<agent_id>`. The realm is the hosted service's name, 1 to 64 bytes of
  `[a-z0-9._-]`. Version 1 has one realm per service and refuses an address naming another.
- **Keys, fingerprints, digests and nonces** in JSON and in headers: lowercase hex; any other spelling
  is refused, never normalized. **Byte strings** in JSON (`enc`,
  `ciphertext`, signatures): base64url without padding (RFC 4648 section 5); a decoder rejects
  padding and characters outside that alphabet.
- **Headers.** A request header whose value is not spelled exactly as this section requires fails
  A.4 step 2 and answers `401`; only the body-size check of step 1 comes before it.
- **Integers** in JSON: numbers from 0 to 2^53 - 1. In binary inputs: `u32` and `u64`, big-endian.
  Key epochs and contact generations are further bounded by A.10.
- **Request bodies** are JSON objects with exactly the members A.6 names for them. An unknown, a
  duplicated or a missing member is `invalid_request`.
- `lp(x)` is a `u16` big-endian length followed by the bytes of `x`.
- `ctx(label)` is the ASCII bytes of `khive-node-v1/` followed by `label` and one zero byte. Every
  signed input in this appendix, and the envelope `header`, `info` and `aad` (A.5), start with their
  own `ctx`, so an input built for one purpose never verifies as an input for another. The plaintext
  and the keys themselves carry none. Two digests are taken over bytes without a
  `ctx`: `SHA-256(body)`, which is only ever signed inside the `ctx("request")` input, and
  `SHA-256(enc || ciphertext)`, which is only compared.

### A.3 Device keys and the enrolment bundle

A delivery device holds two key pairs, generated on the client. Their private halves stay in the
client's key facility; they are never written to a khive store and never sent anywhere.

- a **KEM key**, X25519, used by HPKE (A.5);
- a **signing key**, Ed25519 (RFC 8032, pure), used for request authentication (A.4), receipts (A.6.4)
  and the enrolment proof below.

Two keys, because a receipt must be verifiable by a party other than the sender: the service records
it before answering the sender, and a client may later present it as proof. An HPKE Auth-mode tag
authenticates only to the one recipient that can open it, and an X25519 key does not sign. Both keys
belong to one **key epoch**; replacing either one starts a new epoch. A device's epochs only
increase: a new epoch is numbered above every earlier epoch of that device, and enrolling the same
device again continues its count. A device identifier is never reassigned: enrolment after a
device's directory row is gone mints a new identifier, so a device identifier and a key epoch
together name one key pair.

The **fingerprint** is `SHA-256(ctx("device-keys") || kem_public_key || signing_public_key)`, written
as lowercase hex. It covers both keys, so a directory that substituted either one would change the
value the two owners compare out of band. This supersedes any single-key fingerprint shown before this
appendix was accepted.

The **enrolment bundle** the client hands to the owner's enrolment operation is: the realm,
`kem_public_key`, `signing_public_key`, and `enrol_proof`, an Ed25519 signature by the signing key
over `ctx("enrol") || lp(realm) || kem_public_key || signing_public_key`. The service refuses a
bundle whose keys are not 32 bytes, whose KEM key is one of the X25519 small-order points (checked as
RFC 7748 section 6.1 describes: a shared secret of all zeros is rejected), whose signing key is not a
valid Ed25519 point, or whose proof does not verify. It also refuses a bundle whose KEM key or
signing key is already enrolled for any device in the realm, so no two devices share a fingerprint.
A device enrolled with a KEM key and no signing key cannot authenticate under A.4; its owner enrols
it again with a new pair of keys, because its old KEM key is already enrolled in the realm and would
be refused.

### A.4 Request authentication (the device grant)

A device grant is the device's signing key. There is no bearer secret. Every request under `/node/v1/`
carries four headers:

| header | value |
| --- | --- |
| `Khive-Device` | the device identifier |
| `Khive-Timestamp` | Unix time in whole seconds, decimal: the clock's reading truncated, never rounded up |
| `Khive-Nonce` | 16 random bytes, lowercase hex |
| `Khive-Signature` | base64url Ed25519 signature over the input below |

The signed input is

```text
ctx("request") || device_id (16) || u64(timestamp) || nonce (16)
  || lp(METHOD) || lp(path_and_query) || SHA-256(body)
```

where `path_and_query` is the request target exactly as sent, starting with `/`, and the body digest
of an empty body is the digest of the empty string.

The service authenticates before it parses, in this order: (1) it refuses a body larger than the
limit (A.10) from its declared length or while reading it, before any other step; (2) it requires
the four headers and refuses any request that carries an `Authorization` header, because a tenant API
key or OAuth grant is never a node credential; (3) the device must exist and be active, and so must
its agent and its tenant; (4) the timestamp must be no more than 300 seconds behind the service's
clock and less than 60 seconds ahead of it;
(5) the signature must verify over the raw body bytes with the device's signing key; (6) the
timestamp must not be earlier than the nonce-memory floor, T + 60, where T is the first whole second
at or after S + B, S is the moment the service started remembering nonces (read at the clock's full
precision, never truncated; S may be rounded up to a whole second only when B is a whole number of
seconds, which leaves T unchanged), and B is the clock bound below, 0 only when a single instance,
on one clock that does not run backward across a loss of nonce memory, has held nonce memory;
(7) the nonce, as its 16 decoded bytes, must not have been seen for this device in the last 600
seconds, and is then remembered. Only then does it (8) parse the JSON body. A failure in steps 2 to
5 or step 7 answers `401 unauthenticated` with no further detail. Only a definite answer fails steps
3 and 7: a service that cannot read its directory or its nonce memory answers `503
capacity_exhausted` with `retry_after_seconds`, never `401`. A failure at step 6 answers `503
capacity_exhausted` with `retry_after_seconds` set to 60, a constant, so the answer says nothing about
when the service started. None of these has done any envelope parsing, payload write or charge. Rate limits (`rate_limited`) apply after step 7, per device and per tenant, and
before step 8.

Nonce memory is held by the one instance serving the realm, or shared by every instance that does. A
service that has lost its nonce memory, for example by restarting, cannot tell a request it accepted
before the loss from a replay of one. Every request it accepted before the loss carried a timestamp
less than 60 seconds ahead of its clock at that moment (step 4), so earlier than 60 seconds after the
loss, and step 6 refuses all of those timestamps, because nonce memory starts at or after the loss.
That holds on a service clock that does not run backward across the loss; instances that hold nonce
memory, together or one after another, keep their clocks within a stated bound of one another, which
holds for any two of them, including an instance started after another stopped, and for one instance
before and after a loss of nonce memory, and the floor adds that bound. The service's operator
states that bound, B, and it must be a true bound on those clocks; only the service computes the
floor, so a client never needs B. A replay of a request accepted before the loss
therefore stops at step 6 whatever the signer's clock, and has no effect; a request accepted after
the loss is covered by nonce memory for as long as step 4 can admit it. The signature has already
verified at step 5, so the refusal is retryable: it tells the key holder to sign again. A retry is
always a new request, with a fresh timestamp and nonce and a new signature over the same body bytes.
After a restart a client with an accurate clock is therefore answered `503` for less than 61 seconds
after nonce memory starts, plus twice the clock bound where one applies: once in the floor, and once
because the client's clock may agree with an instance other than the one that started nonce memory.
A relay restart never causes a `401`. A client that receives `401` compares its clock with the
response's `Date` header before it pauses the channel: a clock more than 300 seconds behind, or 60
seconds or more ahead, is the likeliest cause, and the service says no more than the code.

The authenticated device fixes the sender: realm, tenant, agent, device and the device's current key
epoch. A body never supplies any of these. The body fields named in A.6 that look like identities are
expectations the service compares against its own state, and a mismatch is refused; they are never
adopted.

### A.5 Envelope encryption

HPKE (RFC 9180) in Auth mode (`mode_auth`, 0x02), suite DHKEM(X25519, HKDF-SHA256) (KEM 0x0020),
HKDF-SHA256 (KDF 0x0001), ChaCha20-Poly1305 (AEAD 0x0003). One message per HPKE context, sealed at
sequence number 0. The sender's KEM private key is the Auth-mode sender key; the recipient key is the
recipient device's KEM public key at the addressed key epoch.

```text
header = ctx("envelope-header") || u32(protocol_version) || lp(realm)
         || sender_agent_id (16) || sender_device_id (16) || u64(sender_key_epoch)
         || recipient_agent_id (16) || recipient_device_id (16) || u64(recipient_key_epoch)
info   = ctx("envelope") || SHA-256(header)
aad    = ctx("aad") || logical_message_id (16)
```

`info` is 55 bytes, under the 64-byte limit RFC 9180 section 7.2.1 recommends for interoperability
with implementations that allocate these inputs statically. The contact generation is deliberately not in `header`: the service enforces
it on every send (A.6.2), and binding it into the ciphertext would force a re-encryption every time
two agents re-accept each other.

The plaintext is a UTF-8 JSON object: `v` (1), `subject` (string or null), `body` (string),
`sent_at` (RFC 3339, UTC), `thread_id` (a UUID or null), `in_reply_to` (a logical message
identifier or null) and, optionally, `kind` (`announce`, `report` or `ask`: the sender's declared
purpose under runtime ADR-195 D4). A sender writes `kind` only with one of those three values and
otherwise omits it; absent means unspecified. A recipient ignores members it does not know, except that a plaintext with a duplicated member,
or with any of the reserved identity members `from`, `sender`, `to`, `recipient`, `tenant`,
`namespace`, `actor`, `project`, `device` or `delegation`, is invalid and is quarantined (A.8): identity comes from the
authenticated envelope, never from the plaintext. A `kind` with any other value, `null`,
`unspecified` and `reply` included, is likewise invalid and quarantined: a reply is never declared.
A message is a reply when its `in_reply_to` names a verified parent: a message this recipient agent
(the delivery's `recipient_agent_id`) sent to this sender agent (`sender_agent_id`), committed in the
recipient's store as its own outbound message. The recipient evaluates a reply as a reply, whatever
kind it declares. The recipient keeps each outbound message's logical identifier and recipient agent
for as long as it keeps that message's note; a reply whose parent is no longer kept is evaluated by
its declared kind. A follow-up to a message the sender itself sent,
stored or quarantined, is not a reply for policy: it is evaluated by its declared kind, or as
unspecified when it declares none. The `kind` values are fixed for protocol version 1: a recipient
quarantines any other value, so a new value needs a new protocol version. The plaintext need not be canonical JSON,
because nothing is computed over it except the AEAD. There are no attachments in version 1: the
transport carries message notes only (ADR-105, "What stands"). The ciphertext is at most 65,536
bytes, so the plaintext is at most 65,520.

A recipient opens an envelope with its own KEM private key and the sender's KEM public key **as
pinned for that contact at `sender_key_epoch`**, never with a key the service supplies alongside the
delivery.

Auth mode does not resist key-compromise impersonation (RFC 9180 section 9.1.1): whoever holds a
recipient's KEM private key can produce envelopes that open as coming from any of that recipient's
contacts. Protecting that key protects the recipient's inbox, which is one more reason it never
leaves the client's key facility.

### A.6 Endpoints

A success answers JSON. A refusal answers the status code in A.7 with the body
`{"error": "<code>"}`, plus `"retry_after_seconds"` where A.7 says so. No refusal carries free text.

#### A.6.1 `GET /node/v1/contacts/{agent_id}`

Returns the target agent's current delivery key material to an authenticated sender that holds an
active grant for the ordered pair (sender, target):

```json
{"agent_id": "...", "address": "khive1:<realm>/<agent_id>", "device_id": "...", "key_epoch": 2,
 "kem_public_key": "<hex>", "signing_public_key": "<hex>", "fingerprint": "<hex>",
 "contact_generation": 3}
```

No grant, a revoked grant, a block in either direction and an unknown agent all answer
`403 contact_not_active`, so this endpoint is not an existence oracle. The client compares
`fingerprint` with the one its owner pinned when the contact was accepted; on a mismatch it encrypts
nothing to the new key and holds the pair until the owner confirms (A.8).

#### A.6.2 `POST /node/v1/messages`

```json
{"protocol_version": 1, "logical_message_id": "...", "recipient": "khive1:<realm>/<agent_id>",
 "recipient_device_id": "...", "recipient_key_epoch": 2, "sender_key_epoch": 1,
 "contact_generation": 3, "enc": "<b64url, 32 bytes>", "ciphertext": "<b64url>"}
```

After A.4, the service:

1. validates shape and sizes (`enc` decodes to 32 bytes, `ciphertext` to at most 65,536, integers
   within A.10) and `protocol_version`;
2. looks for a recorded receipt (A.6.4) for (sender agent, `logical_message_id`). One recorded for
   the recipient this request names answers `200` with the A.6.5 status object for that message,
   which carries the receipt, whatever the grant, the key epochs or the recipient's connection are
   now, so a submit whose answer was lost is always safe to repeat. The exception is a receipt for
   this request's recipient device and key epoch whose recorded envelope digest differs from this
   request's: that is `envelope_conflict`. One recorded for another recipient is `envelope_conflict`;
3. requires `sender_key_epoch` to equal the authenticated device's epoch, else `invalid_request`;
4. resolves `recipient` inside its realm and requires an active grant for (sender, recipient) at
   exactly `contact_generation` with no block in either direction, else `contact_not_active`. A block
   answers exactly as a missing grant does. A pair's contact generation is never reused: revocation, a
   block, an unblock and a re-acceptance each move the pair to a generation above every earlier one,
   so a delivery admitted at an earlier generation never passes this check again;
5. requires `recipient_device_id` and `recipient_key_epoch` to name the recipient's active device and
   epoch, else `recipient_key_changed`;
6. applies idempotency on (sender agent, `logical_message_id`), with the envelope digest
   `SHA-256(enc || ciphertext)`:
   - the identifier admitted to another recipient within the transport log's 90-day retention
     (C-ADR-033 D8; the log row names the recipient, A.6.6) is `envelope_conflict`;
   - the identifier admitted within that retention to this recipient device and key epoch with a
     different envelope digest (the log row carries the recipient device and the digest, A.6.6) is
     `envelope_conflict`, whether or not that admission is still live;
   - a live admission to this recipient device and key epoch with the same digest answers `200` with
     the body a `202` carries: that admission's `delivery_attempt_id`, state `pending` and original
     `admitted_at`;
   - a live admission to this recipient device and key epoch with a different digest is
     `envelope_conflict`;
   - a live admission to another device of this recipient, or to an older key epoch, is released,
     because the active device cannot open it, and this request continues as a new admission;
   - otherwise the request continues.

   Step 2, this step and the admission record of step 9 are made under one lock on (sender agent,
   `logical_message_id`), which recording a receipt (A.6.4) and releasing a delivery (A.6.3) also
   take. Like nonce memory, the lock is held by the one instance serving the realm or shared by every
   instance that does. A release made while the lock is already held, at this step or on recording a
   receipt, runs under that hold and does not take the lock again. No holder of this lock waits for
   a lock held by anyone waiting for it: a lock taken inside it (the sender's `seq` commit order,
   A.6.3, and whatever serializes step 8's capacity and step 9's charge) is never held while waiting
   for it; a poll may hold its device's one-poll exclusion (A.6.3) when it takes this lock, and no
   holder of this lock waits for that exclusion; and step 7 reads the recipient's connection without
   locking it. A submit that finds the lock held waits for it; a lock that cannot be taken (below) is
   one the service cannot reach, never one another submit holds. So of two concurrent submits naming
   different recipients, never both are admitted: the one whose step 6 reads the other's
   admission is `envelope_conflict`; and no receipt commits and no release, by
   another submit, a receipt, a poll or the 600 seconds running out (A.6.3), happens between a
   submit's step 2 and its step 9;
7. requires a live recipient connection (A.10), else `recipient_offline`, having recorded no
   admission, made no charge and buffered nothing;
8. requires forwarding capacity, else `capacity_exhausted`;
9. mints a `delivery_attempt_id`, then records the admission and its charge together, else
   `insufficient_credit`. The charge is made once per **logical admission key** (sender tenant, sender
   agent, `logical_message_id`) for the life of the account: a resubmission under a key already
   charged is admitted without a new charge;
10. holds the delivery in memory and answers

```json
{"state": "pending", "delivery_attempt_id": "...", "admitted_at": "<RFC 3339>"}
```

with `202`. Admission is not delivery: the sender stays `pending` until it holds a verified receipt
(A.8).

A read that fails never refuses a submit with any code but `503`. A service that cannot read
the receipt record (step 2), the directory (steps 4 and 5) or the transport log (step 6), or cannot
take the lock, answers `503 capacity_exhausted` with `retry_after_seconds`, records no admission and
makes no charge. It never answers as though the read found nothing, and never with a refusal whose
client outcome is `failed` or a hold. A step 9 record that fails, including a balance the service
cannot read, answers the same `503`. So does a step 9 record whose outcome the service cannot
confirm, and that is the one refusal that may follow a committed admission and charge: step 10 has
not run, so nothing is held in memory or forwarded, and A.6.5 answers `unknown`. The client treats
it as any `503` (A.8), and its resubmission of the same bytes is not charged again (step 9). A
sender that cancels the message instead has paid for an admission that was never forwarded.

**Accepted residual.** The logical admission key names no recipient, because the retention decision
this protocol serves keeps only that compact key for the life of an account (C-ADR-033 D7, D8).
Within the 90-day transport-log retention, step 6 refuses an identifier already admitted to another
recipient, and refuses different bytes under an identifier already admitted to the same recipient
device and key epoch, so inside that window one charge buys one envelope per recipient device and key
epoch.
Every admission writes a new log row, so the window runs from an identifier's latest admission. A
sender that leaves an identifier unused for 90 days and then reuses it, for a new recipient or with
new bytes, is admitted without a new charge. A client that follows A.8 never does this, since every
local message gets a fresh random identifier; a client written to break it gets at most one uncharged
envelope per 90 days for each charged identifier, recipient device and key epoch. A conforming
client's re-encryption after `recipient_key_changed` (A.8) uses the same allowance: one envelope for
the new device and key epoch, not charged again.

A **live admission** is the in-memory forwarding record: `enc` and `ciphertext`, their digest,
`protocol_version`, `logical_message_id`, the sender agent, device and key epoch, the recipient agent,
device and key epoch, `contact_generation`, the `delivery_attempt_id`, `admitted_at`, and the times of
its first and its latest hand-out (A.6.3). Every field a delivery (A.6.3) carries and a receipt
(A.6.4) is checked against comes from it. It ends when its receipt is recorded, when it is released
(A.6.2 step 6, A.6.3), or when the service restarts. It is never written to storage (A.6.6).

#### A.6.3 `GET /node/v1/poll?wait=<seconds>&receipts_after=<n>`

One outstanding poll per device; a second is `poll_in_progress`. The poll returns as soon as a
delivery or a receipt is available for the device, or when `wait` (0 to 25) elapses:

```json
{"deliveries": [ ... ], "receipts": [ ... ], "receipts_cursor": 41, "server_time": "<RFC 3339>"}
```

A **delivery** carries everything the recipient needs to open, verify and bind a receipt:

```json
{"delivery_attempt_id": "...", "logical_message_id": "...", "protocol_version": 1,
 "sender_agent_id": "...", "sender_device_id": "...", "sender_key_epoch": 1,
 "recipient_agent_id": "...", "recipient_device_id": "...", "recipient_key_epoch": 2,
 "contact_generation": 3, "enc": "<b64url>", "ciphertext": "<b64url>"}
```

The sender fields are the ones the service derived at A.4, never ones the submitter wrote. A delivery
carries no sender address: the client derives it from `sender_agent_id` and its own realm (A.9).

Before every hand-out, first or repeated, the service checks that the grant is still active at the
delivery's `contact_generation` with no block in either direction (the A.6.2 step 4 predicate), and
that the delivery's `recipient_key_epoch` is still the recipient device's active epoch. A delivery that
fails either check is released and never handed out again. The one exception is a revocation or a
block after a first hand-out: neither can retract bytes already disclosed, so a delivery already
handed out to the device before it may be handed out to that device again, and receipted, until it is
receipted or released. Nothing is handed out for the first time after a revocation or a block. A delivery that was handed out and has
no receipt is handed out again, under the same `delivery_attempt_id`, after 30 seconds. The service
releases a delivery's memory when its receipt is recorded, or 600 seconds after admission without
one; the sender then resubmits the same bytes (A.8), and the resubmission is not charged again. A
resubmission with different bytes for the same device and key epoch is `envelope_conflict` (A.6.2
step 6).

A **receipt** item is `{"seq": n, "receipt": {...}, "recorded_at": "<RFC 3339>"}`, where `receipt` has
the shape posted in A.6.4. The service returns receipts for which the device's agent is the sender,
with `seq` greater than `receipts_after`, within the retention of the durable receipt record. `seq` is
assigned when the durable receipt commits, one sender agent at a time, so `seq` order is commit order
and a poll never returns `n + 1` while `n` is still to commit. A sender's `seq` values only increase
and are never reused, even after every receipt of that sender has passed its retention: `seq` is
assigned above that sender's receipt high-water (A.6.6), not above the highest receipt still kept.
`receipts_cursor` is the `seq` of the
last receipt in the page, or `receipts_after` when the page carries none. A client advances its stored
`receipts_after` past a receipt once it has either verified it and durably recorded the outcome, or
rejected it and reported it (A.8), so one receipt that fails verification cannot hold back the ones
after it.

#### A.6.4 `POST /node/v1/receipts`

```json
{"binding": {"protocol_version": 1, "logical_message_id": "...", "sender_agent_id": "...",
             "recipient_agent_id": "...", "recipient_device_id": "...", "recipient_key_epoch": 2,
             "contact_generation": 3, "delivery_attempt_id": "..."},
 "disposition": "stored", "signature": "<b64url>"}
```

The signature is Ed25519, by the recipient device's signing key, over

```text
ctx("receipt") || u32(protocol_version) || logical_message_id (16) || sender_agent_id (16)
  || recipient_agent_id (16) || recipient_device_id (16) || u64(recipient_key_epoch)
  || u64(contact_generation) || delivery_attempt_id (16) || disposition (1 byte: 1 stored, 2 quarantined)
```

A receipt equal to one already recorded, posted by that receipt's recipient device, answers
`200 {"recorded": true}` again. Otherwise the service requires the authenticated device to be `recipient_device_id` and its agent to be
`recipient_agent_id`, else `receipt_invalid`; requires (`logical_message_id`, `delivery_attempt_id`) to name a live admission
(A.6.2) to this recipient, else `not_found`; requires every binding field to equal what that admission recorded,
and the signature to verify with the device's signing key at `recipient_key_epoch`, else
`receipt_invalid`. It records the durable receipt before it answers `200 {"recorded": true}`, and only
then releases the delivery. Recording assigns the receipt its `seq` (A.6.3). A valid receipt whose
disposition differs from one already recorded for the same logical message is `receipt_conflict`, and
the first recorded disposition stands. A record the service cannot read or write, or the lock it
cannot take, answers `503 capacity_exhausted` with `retry_after_seconds`, never `not_found`: the
recipient retries the same bytes (A.8), and a receipt that did commit answers `200 {"recorded": true}`
again.

#### A.6.5 `GET /node/v1/messages/{logical_message_id}`

Answers only about a logical message the authenticated device's agent sent. Any other identifier,
including one another agent sent, answers `unknown` exactly as one never seen does:

```json
{"logical_message_id": "...", "state": "pending", "delivery_attempt_id": "...", "receipt": null}
```

`state` is `pending` while an admission is live, `recipient_stored` or `recipient_quarantined` with
the receipt when one is recorded, and `unknown` otherwise. The service never answers `failed`:
`failed` is the client's own classification of a permanent refusal. An identifier the service has no
record of answers `200` with `unknown`, not `404`, because absence of a record is not evidence that
the message was not delivered.

#### A.6.6 What the service keeps

Every stored field falls in one of the categories of the server-log allowlist in the product decision
this protocol serves, C-ADR-033 D8 as amended 2026-09-23 (Amendment 1: the transport log's admission row
carries the recipient device and the envelope digest as replay identity, and the durable receipt
carries its commit-order position), and nothing else about a message is kept:

- **Directory state**: agents, each with its receipt high-water (the highest `seq` assigned to it as a
  sender, A.6.3, kept while the agent exists and so beyond the retention of its receipts; it names no
  recipient), devices (both public keys, key epoch, status), grants, blocks and requests.
- **Transport log**, one row per admission or per refusal of an authenticated request (a request
  refused at A.4 writes none), kept 90 days: the `delivery_attempt_id`, a
  random identifier minted at admission, as the transport request identifier (a refusal row has
  none); the authenticated realm, tenant, agent and device; the recipient realm, tenant and agent,
  in every admission row (the step 4 grant is the authorization C-ADR-033 D8 asks for) and in a
  refusal row only when the request passed step 4; `logical_message_id`; `protocol_version`;
  `contact_generation`; `recipient_key_epoch`;
  in an admission row only, the recipient device identifier and the envelope digest
  `SHA-256(enc || ciphertext)`, as replay identity (key epochs are counted per device and only
  increase, A.3 and A.8, so an epoch names a key only together with its device); the ciphertext byte count; the delivery state; the closed refusal code; the retry count; the
  admission, forward and receipt times; and the billing reference. Step 6 of A.6.2 reads
  the admission rows for the recipient, recipient device, key epoch and digest of each admission of an
  identifier within retention. It and the durable receipt, each kept 90 days, are the only records
  that name the recipient of a message; no longer-lived record does.
- **Durable receipt**: the receipt's binding fields, disposition and signature (the values, not the
  request that carried them), the envelope digest, the time it was recorded, and `seq`, its position
  in its sender's commit order (A.6.3), which states nothing about the message beyond that order.
- **Accounting**: the logical admission key (A.6.2 step 9), which names no recipient, the payer,
  the cost class, the plan or credit disposition, the amount and the admission time.
- **Memory only, never written**: live admissions (A.6.2), nonce memory (A.4), and open polls.

A restart therefore loses only forwarding state: live admissions, which senders resubmit (A.8), and
nonce memory, which A.4 covers.

### A.7 Refusal codes

The set is closed. A client treats an unknown code as `invalid_request`.

| status | code | meaning | client outcome |
| --- | --- | --- | --- |
| 400 | `invalid_request` | malformed body, field or encoding; sender epoch mismatch | `failed` |
| 400 | `unsupported_version` | `protocol_version` is not 1 | `failed` |
| 401 | `unauthenticated` | an A.4 failure at steps 2 to 5 or step 7 | pause the channel, keep pending rows |
| 402 | `insufficient_credit` | the sender's account cannot pay for the admission | held until the owner acts |
| 403 | `contact_not_active` | no active grant at that generation, or a block | `failed` until a verified receipt says otherwise (A.8) |
| 404 | `not_found` | a receipt names no live admission to this recipient | drop the journal entry (A.8) |
| 409 | `recipient_offline` | no live recipient connection; free, `retry_after_seconds` | stay `pending`, retry |
| 409 | `recipient_key_changed` | the recipient's device or epoch changed | held until the owner confirms |
| 409 | `envelope_conflict` | same logical message: a different envelope at the same epoch, or another recipient | `failed` |
| 409 | `receipt_conflict` | a different disposition is already recorded | keep local state, report |
| 409 | `poll_in_progress` | the device already has a poll open | retry after the open one ends |
| 413 | `payload_too_large` | body or ciphertext over the limit | `failed` |
| 422 | `receipt_invalid` | binding mismatch or bad signature | report; do not retry the same bytes |
| 429 | `rate_limited` | a request-rate limit; `retry_after_seconds` | retry |
| 503 | `capacity_exhausted` | forwarding memory full; a correctly signed request timestamped before the nonce-memory floor (A.4 step 6); or a record the service cannot read or write, or a lock it cannot take (A.4 steps 3 and 7, A.6.2, A.6.4); free, except a step 9 record whose outcome the service cannot confirm, which may have charged (A.6.2); `retry_after_seconds` | stay `pending`, retry with a new signature |

No refusal records an admission or makes a charge, except the one `503` A.6.2 names for a step 9
record whose outcome the service cannot confirm.

### A.8 Client obligations

- **Before the first submit** the runtime durably records the logical message identifier (a random
  UUID, fresh for each local message), the recipient address, the recipient device identifier, key
  epoch and fingerprint used, the contact generation, the sender key epoch, `enc` and `ciphertext`.
  Every retry sends exactly those bytes. The only re-encryption under the same logical identifier is
  after `recipient_key_changed` and the owner's confirmation of the new fingerprint: that is a new
  (identifier, device, epoch) key, not a conflict. The runtime therefore keys a sender's envelope by
  (`logical_message_id`, `recipient_device_id`, `recipient_key_epoch`), holds at most one envelope
  per key (A.6.2 step 6 refuses a second one within the transport log's retention, and step 2 once a
  receipt is recorded), and treats the most recently confirmed envelope as the current one. Key
  epochs are counted per device, so a replacement device's epoch says nothing about its order
  against the old device's.
- **Held messages.** After `insufficient_credit` or `recipient_key_changed` a message stays `pending`
  with a hold reason that suspends automatic retry until the owner acts. The transport-status
  operation (amendment item 7) reports a held message as `pending`; a hold is never a further state.
  The service holds no live admission for a refused submit (its transport-log row records only the
  refusal; a step 9 record it could not confirm, A.6.2, may have committed but holds nothing in
  memory), so A.6.5 answers `unknown` for a message with no live admission and no recorded receipt.
- **Policy holds.** The runtime's pair policy (ADR-195 D6) is evaluated before every submission, a
  resubmission after admission included. Under `enforce` a refusal makes no submission and holds the
  message with the hold reason `policy_denied`; under `shadow` a refusal is audited and the submission
  goes ahead; under `off` nothing is evaluated (ADR-195 D7). Unlike the holds above, a policy hold is
  entered by the client, not answered by the service, so it can follow an admission. The hold charges
  nothing further and recalls nothing: an earlier admission of the message may still be delivered, a
  verified receipt from a poll or a status read ends the hold as it ends any `pending`, and a message
  answered `202`, or answered in a way that may have charged (Retries, below), is still shown as
  charged or possibly charged. An earlier admission can also be stored with no receipt ever
  recorded (its receipt post answered `not_found` after the release); that message stays `pending`
  under the hold until an evaluation admits it, because only a resubmission obtains its receipt. A
  `policy_denied` hold after a `202`, or after a `200` that shows the message `pending`, therefore never
  means undelivered, and the client says so to the
  owner before a cancel. The hold suspends automatic retry, the status-read resubmission included.
  A hold is written only to a message still `pending`: a receipt verified between the refusing
  evaluation and the hold write stands, and no hold is written. The hold records the policy state
  its own evaluation read: the mode and the revision. The revision advances on every change ADR-195
  D9 lets an administrator make, a removal included (an actor record, a class, an address binding, a
  rule or the mode), and an evaluation reads it in the same snapshot as the rules and records it
  uses, or before them. A legacy route's expiry changes decisions without an administrator; it only
  removes an allow, so it never frees a held message. A transport attempt evaluates the sender's
  assurance recorded when the message was sent. The message is evaluated again once whenever the current state
  differs from the recorded one, whether the change came before or after the hold was written, and
  nothing else retries it; an evaluation that admits it submits it, and one that refuses it records
  the state that evaluation read. A change of mode is a change of state, so a move to `off` or
  `shadow` releases every policy hold to submission. The policy store cannot be read only when it
  answers with an error, as defined in Receiving, step 4. When it cannot be read before a
  submission, no submission is made and the message stays `pending` on the backoff used after a
  5xx; no submission was made, so this adds no charge. It is neither held nor submitted
  unevaluated, and a held message whose re-evaluation cannot read the store leaves the hold for that
  backoff.
- **Retries** after `recipient_offline`, `capacity_exhausted`, `rate_limited`, a 5xx or a network
  failure use exponential backoff from 30 seconds to at most one hour, with jitter, so an offline pair
  does not turn one message into a stream of refusals. Every retry is a new request under A.4: a
  fresh timestamp and nonce and a new signature over the same body bytes. A submit answered `503` or
  another 5xx, or not answered at all, may have been charged (A.6.2). The client shows that message
  as possibly charged, never as free, until a `202`, or a `200` that answers a submit of that message
  or shows it `pending` or receipted, shows the charge; a refusal settles nothing (C-ADR-033 D7). A
  runtime that cannot tell these failures apart shows every one of them as possibly charged.
- **Resubmission after admission.** A `202` is not the end of the sender's work. The client resubmits
  the same bytes, under the same backoff, when a status read (A.6.5) answers `unknown` for a message
  it holds as `pending`, and in any case once 600 seconds have passed since `admitted_at` without a
  verified receipt. A service restart, a release without a receipt and a receipt lost in transit all
  end the same way: the resubmission is not charged again. It is answered from the recorded receipt
  (A.6.2 step 2) when there is one, and otherwise goes through A.6.2 as any submit does.
- **Verifying a receipt (sender).** A receipt, whether it arrives in a submit answer, a poll or a
  status read, changes nothing until the sender has checked it: (1) every binding field equals the
  outbox record: `protocol_version`, `logical_message_id`, `sender_agent_id` (the sender itself),
  `recipient_agent_id`, `recipient_device_id`, `recipient_key_epoch` and `contact_generation`, and
  `delivery_attempt_id` is a well-formed identifier; (2) the A.6.4 signature verifies with the
  recipient's signing public key **as pinned by the owner for that contact at `recipient_key_epoch`**,
  never a key the service supplies with the receipt. A receipt that fails either check leaves the
  message `pending` and is reported, and a poll cursor still advances past it (A.6.3). A verified
  receipt is final: it overrides a `failed` classification made earlier, for example after
  `contact_not_active` on a retry whose first answer was lost. The outbox record therefore outlives a
  `failed` classification, for at least as long as the service retains receipts (C-ADR-033 D8).
- **Receiving.** For each delivery, in this order:
  1. the recipient fields must name this client's agent, device and active key epoch; a delivery that
     does not gets no receipt;
  2. the sender must be a contact whose owner-pinned fingerprint is at `sender_key_epoch`. A delivery
     from an agent that is not a contact gets no receipt and is dropped. A delivery from an epoch the
     owner has not confirmed is held locally without being opened, gets no receipt and claims no
     replay identity, counts against the local quarantine bound, and is processed again from step 1
     once the owner confirms that fingerprint, or discarded if the owner declines;
  3. the envelope must open (A.5). One that does not open gets no receipt and claims no replay
     identity: the relay can hand the recipient any bytes, and only an authenticated envelope may
     settle what happened to a sender's message. The client may keep it locally under
     (`delivery_attempt_id`, `SHA-256(enc || ciphertext)`) for diagnosis, within the local quarantine
     bound. The client sets that bound; past it the oldest held or quarantined item, other than a
     policy-refused one (step 4), is dropped and reported;
  4. a delivery whose replay identity is already claimed is answered from it (below), and is neither
     parsed again nor evaluated against the pair policy. Otherwise the plaintext is parsed, and a
     valid one is evaluated against the recipient's pair policy (ADR-195 D8); under `off` nothing is
     evaluated. A valid one the policy admits, or refuses under `shadow` (the refusal is audited), or
     any valid one under `off`, is committed as the message note together with the transport
     receipt record in one transaction, and the client then signs a `stored` receipt. The note
     records the kind the message carries for policy: the declared value, `unspecified` when none
     is declared, or `reply` when it is derived. An invalid one (not the A.5 object, a duplicated
     member, a reserved identity member, or a `kind` outside `announce`, `report` and `ask`), or a
     valid one the policy refuses under `enforce`, is kept as received (the delivery item verbatim,
     as one JSON object) in local quarantine storage with a closed reason, that record is committed,
     and the client signs a `quarantined` receipt. A policy-refused item is also kept with its parsed
     plaintext, and not under the bound of step 3: the client keeps a bound per sender, past which
     the oldest of that sender's policy-refused items is dropped and reported, so one sender's
     refusals never evict another's. The replay identity is claimed inside the commit's transaction,
     on one identity shared by the message note and the quarantine record: a commit that finds it
     already claimed lands nothing and is answered from the claim. When
     the policy store cannot be read, nothing is committed and no receipt is sent, as for a failed
     write, and the runtime reports it where it reports a failed write. The message is processed
     again from step 1 when it next arrives: as the relay's next hand-out while its admission is
     live, or as the sender's resubmission after the release (A.10). The store cannot be read only
     when it answers with an error: an actor with no record is `unclassified` (ADR-195 D3), which
     is a decision, and a store holding no policy state is in mode `off`.
  `quarantined` therefore always means: authenticated as the pinned sender, and not acceptable as a
  message.
- **Replay identity** is (`sender_agent_id`, `logical_message_id`), and only a commit in step 4 claims
  it. The answer below is given only to a delivery that has passed steps 1 to 3: a delivery is never
  answered from replay identity before it has opened. A later delivery of a logical message already committed, under any attempt identifier, lands no
  message note and no new receipt record; it adds one acknowledgement-journal entry for the new
  attempt's binding, carrying the disposition first recorded, and that entry is the receipt sent.
- **Receipts** are sent from a durable acknowledgement journal and retried with the same bytes after a
  5xx or a network failure. After `not_found` (the service no longer holds that attempt) the entry is
  dropped; the sender's resubmission arrives as a new attempt and is answered through replay
  identity. Polling never acknowledges.
- **Own key rotation.** Version 1 has no re-encryption under a new sender key epoch. A device does not
  rotate its own keys while it has messages pending: the runtime asks the owner to let them resolve
  or to cancel them first, and a cancelled message is `failed`.

### A.9 Mapping onto the runtime

The channel kind is `khive`. There is one configured slug per enrolled local agent. The slug's
configuration names the realm, the service base URL, the agent identifier, the device identifier, and
references into the key facility (never key bytes), and the local actor it serves. An outbound comm
message addressed to `khive1:<realm>/<agent_id>` from an actor bound to a slug of that realm goes
through that slug; with no such slug it is not routed. The recipient of an inbound message is the
slug's bound actor, fixed by transport authority and never read from the payload. The sender is shown
by the address the client derives from `sender_agent_id` and the slug's realm; a display name for a contact is a local property of the client.

The transport-status operation (amendment item 7) reports `pending`, `recipient_stored`,
`recipient_quarantined`, `failed` or `unknown`, from the runtime's own records and receipts.

Node outbox rows live in the runtime-owned transport state of the amendment's item 7. Their
`failed` is that state's classification, not the terminal mark the existing outbound-note path
writes, and it moves to `recipient_stored` or `recipient_quarantined` when a verified receipt
arrives (A.8).

`SendOutcome::Pending` carries a companion value: the `admitted_at` of the admission when the
service answered one, the hold reason when the answer was a hold (A.8), and the reason when a receipt
failed verification. The runtime records these in its item 7 state; the adapter writes no state. At
khive-oss c160b2f2 `Pending` is a unit variant (`crates/khive-channel/src/lib.rs`) and the
amendment's item 5 names it bare, so this changes `khive-channel`'s public enum. A follow-up
`khive-channel` change makes it (the receipt additions are already in the tree at c160b2f2), under
the runtime's own review, before the node adapter (C-ADR-033 D12 orders the `khive-channel` receipt
additions, its step (1), before the node adapter, its step (2); this change goes with step (1)). Existing adapters answer `LegacyAccepted` through the default method and
construct no `Pending`, so they stay byte-identical. Service answers map onto the channel's results by the A.7 client outcome:
- `202`, and a `200` whose state is `pending`, are `SendOutcome::Pending` with that `admitted_at`; the
  row then waits for the A.8 resubmission 600 seconds after it, not for the retry backoff;
- a `200` carrying a receipt is `SendOutcome::RecipientStored` or `SendOutcome::RecipientQuarantined`
  only after the A.8 verification passes. One that fails it is `SendOutcome::Pending` with the
  reason, which the runtime reports, and the row resubmits under the A.8 backoff;
- a refusal whose outcome is to retry, a 5xx and a network failure are `ChannelError::Transport`, and
  the row stays pending under the A.8 backoff;
- a held outcome is `SendOutcome::Pending` with its hold reason, and the row suspends retry until the
  owner acts (A.8);
- a submission the runtime's pair policy refuses (ADR-195 D6) never reaches the adapter: the row
  stays `pending` with the hold reason `policy_denied` until a verified receipt arrives or a later
  evaluation admits it (A.8, policy holds). A submission for which the policy store cannot be read
  never reaches the adapter either, and the row stays pending under the A.8 backoff;
- `401` is `ChannelError::Auth`, handled as the amendment's item 8 requires: the channel pauses for
  credential repair and its pending rows are kept, never classified `failed`;
- a refusal whose outcome is `failed` is `ChannelError::PermanentTransport`, which classifies the row
  `failed` and keeps its outbox record (A.8).

On the receipt path `acknowledge_receipt` answers, for each acknowledgement-journal entry:
- `Ok(())` for `200 {"recorded": true}`, and the entry is done;
- `Ok(())` for `not_found`, and the entry is dropped (A.8);
- `ChannelError::PermanentTransport` for `receipt_conflict`, `receipt_invalid`, and any other refusal
  whose A.7 outcome is `failed` (for example an edge answering `400` or `413`): the entry is retired
  and reported, never retried with the same bytes, and the local message note and receipt record are
  kept;
- `ChannelError::Transport` for a refusal whose outcome is to retry, a 5xx or a network failure, and
  the entry is retried with the same bytes;
- `ChannelError::Auth` for `401`, handled as above, and the entry is kept.

### A.10 Limits (version 1)

| limit | value |
| --- | --- |
| request body | 98,304 bytes |
| ciphertext | 65,536 bytes |
| poll `wait` | 0 to 25 seconds |
| deliveries per poll | 16 |
| receipts per poll | 64 |
| timestamp skew | at most 300 seconds behind the service's clock, less than 60 seconds ahead |
| nonce-memory floor | 60 seconds (the forward skew) after the first whole second at or after the moment nonce memory started, that moment taken plus the clock bound B (A.4) |
| nonce memory | at least 600 seconds |
| redelivery of an unreceipted hand-out | 30 seconds |
| forwarding memory released without a receipt | 600 seconds after admission |
| live recipient connection | a poll open now, or ended less than 30 seconds ago |
| key epoch, contact generation | 1 to 4,294,967,295 (encoded as `u64`; a larger value is `invalid_request`) |

The service may set a lower value for any maximum here and never a higher one within version 1,
except the two skew values and the floor's 60 seconds, which are fixed in version 1: the 60 seconds
is the forward skew, and a client reads both skew values when it diagnoses a `401`. The one minimum, nonce memory,
may be raised and never lowered.

### A.11 Test vectors

Produced by a reference implementation that reproduces every value of RFC 9180 Appendix A.2.3
(Auth mode, this suite), including the intermediate key schedule and the sequence 0 and 1
ciphertexts. X25519 key pairs come from RFC 9180 `DeriveKeyPair(ikm)`; Ed25519 key pairs from the
32-byte seed (RFC 8032). Every value is lowercase hex unless labelled otherwise.

**Device keys, sender.**

```text
kem_ikm            = fd210f75cf232986830293821ab1befbab87049b3731e41ab1cb5c2330125f05
kem_public_key     = 819776ce19bb5834bf394fc9a773dde61352dca90d69b9dcce03eac87266dd61
signing_seed       = 3f2e106651676d6280d30ce668e441015f59c93ad92c07680990f45cafc5db30
signing_public_key = 9016672157bdb5b3529477312593f8e6fbf59641a52a374d50bd72fdf0f5d2af
fingerprint        = 55ed66d1dd6c913908d0120a4aed782688b6263c44eaeb2c3105ba8cc1a0a30a
enrol_proof_input  = 6b686976652d6e6f64652d76312f656e726f6c00000d72656c61792e6578616d706c65819776ce19bb5834bf394fc9a773dde61352dca90d69b9dcce03eac87266dd619016672157bdb5b3529477312593f8e6fbf59641a52a374d50bd72fdf0f5d2af
enrol_proof        = 78abd2a15054bd9ca627020372e9de0e989a6682f1c1c872de870e5019bfe679b547c3c1b6a29fa11eb970071155ea69561494b01caaf015118969512b481b05
```

**Device keys, recipient.**

```text
kem_ikm            = 86490e4131529686a1872dadf9568f43b2b7da134a9a67f4a2b7551667d90a5d
kem_public_key     = 4d9256a0694825d0f9db96dbc74b10ddaebd6109a372b5b0d810bfda7a60873c
signing_seed       = 4cd342ea22d005991e18dd7445d4d360e7c99f165db74ef0896ea143cca15871
signing_public_key = a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032
fingerprint        = 5c07d31afa160697a0d47ad4528bb300371b5845c3dddd51c4a752fc998a4e6a
enrol_proof_input  = 6b686976652d6e6f64652d76312f656e726f6c00000d72656c61792e6578616d706c654d9256a0694825d0f9db96dbc74b10ddaebd6109a372b5b0d810bfda7a60873ca914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032
enrol_proof        = c709237677f27e67ad3a1cf4faaf8d91a44ace2f3d543979223587981e3fd64a9478d86ff0944062350915e3b185dab6e4511c82ee6119f382b9fd5568e5b109
```

**Envelope.** Realm `relay.example`, protocol version 1.

```text
sender_agent_id     = 01920000-0000-7000-8000-00000000a001
sender_device_id    = 01920000-0000-7000-8000-00000000d001
sender_key_epoch    = 1
recipient_agent_id  = 01920000-0000-7000-8000-00000000a002
recipient_device_id = 01920000-0000-7000-8000-00000000d002
recipient_key_epoch = 2
logical_message_id  = 6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5
header              = 6b686976652d6e6f64652d76312f656e76656c6f70652d6865616465720000000001000d72656c61792e6578616d706c650192000000007000800000000000a0010192000000007000800000000000d00100000000000000010192000000007000800000000000a0020192000000007000800000000000d0020000000000000002
info                = 6b686976652d6e6f64652d76312f656e76656c6f70650034ff8888ea2f1f87aa1930c65852ee20f795a7f17c5f769695d0ad675a0aa663
aad                 = 6b686976652d6e6f64652d76312f616164006f1c2d3e4a5b4c6d8e7f90a1b2c3d4e5
plaintext           = 7b22626f6479223a2244696420746865207265706f727420676f206f757420746f6461793f222c22696e5f7265706c795f746f223a6e756c6c2c2273656e745f6174223a22323032362d30392d32335432303a30303a30305a222c227375626a656374223a226461696c7920636865636b222c227468726561645f6964223a6e756c6c2c2276223a317d
ephemeral_ikm       = 0bf6cba75c774ca7b00e959d4e882bdf4160921c7c3f5c8caaedd9b9c657bd1f
enc                 = 797b15e6209ab045e8aa1336741ff7d1182ad43d6d5823537763521559a5cb44
shared_secret       = 04dc45cdbcefd4a719ecf0c58b31ec795288097ed994e1a8b106405f6e502a48
ciphertext          = a112e259c81cdca6c024aa92da620afc09ce61875d176654918a208e95bb161f79dd47da237dfbd209852c2bb11651bc17fdcab2f3e6c79892030b602b684b4538697410d208636762ffe11281824b5fe20d2dbb310d3ed52bc596e2aea2539d5c3614a6d00e518415c488914bac422f77d0d2718a4dc0525e59a09874a369e8dbb33f0d4bd5dd191b4acac958af12a99bee5ae44d5cf020d672
ciphertext_len      = 154
```

The plaintext above is the UTF-8 bytes of:

```json
{"body":"Did the report go out today?","in_reply_to":null,"sent_at":"2026-09-23T20:00:00Z","subject":"daily check","thread_id":null,"v":1}
```

**Receipt binding.**

```text
contact_generation  = 3
delivery_attempt_id = 01920000-0000-7000-8000-0000000e0001
logical_message_id  = 6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5
protocol_version    = 1
recipient_agent_id  = 01920000-0000-7000-8000-00000000a002
recipient_device_id = 01920000-0000-7000-8000-00000000d002
recipient_key_epoch = 2
sender_agent_id     = 01920000-0000-7000-8000-00000000a001
```

**Receipt, disposition `stored`,** signed with the recipient's signing key.

```text
signing_input = 6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000101
signature     = 00de06d16479cd2a99fd6e66ae90ad66716c9af015fcba6b65db52029cbbe83bc9161dac398929cfb7955e3eb3cd8f4ae69e2cde7331d9aeb2a1f597a6e12f01
```

**Receipt, disposition `quarantined`,** signed with the recipient's signing key.

```text
signing_input = 6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000102
signature     = 1925fea98935bc24b684df54d625b60748de38292a55a6215802607e51b44fda8190120b7b5460562e1ab3404c7096931144ab6ac2d3201e1b5543b96939aa0c
```

**Request authentication**, the sender submitting the envelope above.

```text
device_id      = 01920000-0000-7000-8000-00000000d001
timestamp      = 1790193600
nonce          = bc13f0c305a174efd0b8bd7ea60a0596
method         = POST
path_and_query = /node/v1/messages
body_sha256    = 9b5e8b624acf924909794dfa88eed3a830e7336a851e2561e29a1bd90c51e601
signing_input  = 6b686976652d6e6f64652d76312f72657175657374000192000000007000800000000000d001000000006ab42fc0bc13f0c305a174efd0b8bd7ea60a05960004504f535400112f6e6f64652f76312f6d657373616765739b5e8b624acf924909794dfa88eed3a830e7336a851e2561e29a1bd90c51e601
signature      = 7c301670988085c24a253e0537e7ca000e96ef7a164c567cbda6ad71ad34b1d8ff2c6e6abdb48d2ea9dce25c1d63acf4e7d5af0d5e8efbf59a60ede336825d0f
```

The request body above is the UTF-8 bytes of:

```json
{"ciphertext":"oRLiWcgc3KbAJKqS2mIK_AnOYYddF2ZUkYogjpW7Fh953UfaI3370gmFLCuxFlG8F_3KsvPmx5iSAwtgK2hLRThpdBDSCGNnYv_hEoGCS1_iDS27MQ0-1SvFluKuolOdXDYUptAOUYQVxIiRS6xCL3fQ0nGKTcBSXlmgmHSjaejbsz8NS9XdGRtKyslYrxKpm-5a5E1c8CDWcg","contact_generation":3,"enc":"eXsV5iCasEXoqhM2dB_30Rgq1D1tWCNTd2NSFVmly0Q","logical_message_id":"6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5","protocol_version":1,"recipient":"khive1:relay.example/01920000-0000-7000-8000-00000000a002","recipient_device_id":"01920000-0000-7000-8000-00000000d002","recipient_key_epoch":2,"sender_key_epoch":1}
```
**Negative vectors.** Each of these must be refused by a conforming implementation.

| input | required outcome |
| --- | --- |
| the envelope with the last ciphertext byte XORed with 0x01 | open fails (AEAD tag) |
| the envelope opened with `info` built from `recipient_key_epoch` = 3 | open fails |
| the envelope opened with `aad` for logical message `6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e6` | open fails |
| the envelope opened with a sender KEM key other than the pinned one (`DeriveKeyPair(SHA-256("khive-node-v1 vector impostor kem"))`) | open fails |
| the `stored` receipt signature checked against the `quarantined` signing input | verify fails |
| the `stored` receipt signature checked against a binding with `delivery_attempt_id` = `01920000-0000-7000-8000-0000000e0002` | verify fails |
| the `stored` receipt signature checked with the sender's signing key | verify fails |
| the `stored` receipt signature checked against a binding with `logical_message_id` = `6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e6` | verify fails |
| the `stored` receipt signature checked against a binding with `sender_agent_id` = `01920000-0000-7000-8000-00000000a003` | verify fails |
| the request signature checked against `path_and_query` = `/node/v1/receipts` | verify fails |
| the request signature checked against the body with one space appended | verify fails |

**Conformance cases.** These are behaviours, not byte values; each is a required test of an
implementation.

| case | required outcome |
| --- | --- |
| a delivery under a genuine binding whose envelope does not open | no receipt, no replay identity; a later authentic delivery of the same logical message is stored |
| a delivery from a sender key epoch the owner has not confirmed | held unopened, no receipt, until the owner confirms |
| a receipt whose signature is valid but whose binding names another logical message or another sender | the sender's message stays `pending` |
| a receipt signed with a key the service supplies rather than the pinned one | the sender's message stays `pending` |
| a submit repeated after its answer was lost, the recipient having since revoked the grant | `200` with the recorded receipt |
| a submit of a logical message already admitted to another recipient | `envelope_conflict` |
| a plaintext carrying a `from` member, or a duplicated member | `quarantined` receipt, no message note |
| a plaintext whose `kind` is `null`, `reply` or any value other than `announce`, `report` and `ask` | `quarantined` receipt, no message note |
| a plaintext with `kind` `announce` that the recipient's pair policy admits | `stored` receipt, and the message note records the kind |
| a valid plaintext the recipient's pair policy refuses under `enforce` | `quarantined` receipt, the item in local quarantine with its parsed plaintext, no message note |
| a delivery arriving while the recipient's policy store cannot be read | no commit, no receipt; once the store reads again, the next arrival of the message is stored and receipted exactly once |
| a logical message stored, its receipt dropped after `not_found`, delivered again under a new attempt after the recipient's policy changed to refuse it | no quarantine item; the journal entry for the new attempt carries `stored` |
| a pending message the sender's pair policy refuses under `enforce` before a submission | no submission; `pending` with the hold reason `policy_denied`; while held, no retry and no status-read resubmission; a later evaluation that admits it submits it |
| a message answered `202`, then refused by the sender's pair policy at its resubmission, whose earlier admission the recipient stores | no resubmission; the verified receipt from the next poll moves it to `recipient_stored`; shown as charged throughout |
| a policy revision that admits the pair, published after the refusing evaluation and before its hold is written (a runtime test with a seam at that point) | the message is evaluated under the new revision and submitted |
| messages held for `policy_denied` under `enforce` when the mode moves to `off` or `shadow` | submitted, with no revision change |
| a pending message whose next submission finds the sender's policy store unreadable | no submission, no hold; once the store reads again, the next backoff step evaluates it and, if admitted, submits it, with no revision change |
| a message held because its recipient's class was changed, the class then restored with no rule edited | evaluated again and submitted |
| a verified receipt recorded between a refusing evaluation and its hold write (a runtime test with a seam at that point) | `recipient_stored`, no hold |
| a held message refused again at a re-evaluation | no further evaluation until the state differs again |
| a held message whose re-evaluation cannot read the sender's policy store | leaves the hold; no submission until the store reads, then retried on the backoff |
| a plaintext declaring `ask` whose `in_reply_to` names the sender's own earlier message, which the recipient stored, under a recipient policy that allows `reply` and denies `ask` | evaluated as `ask`: `quarantined` receipt |
| a plaintext declaring `ask` whose `in_reply_to` names a message the recipient sent to this sender, under the same policy | evaluated as a reply: `stored` receipt, the note records `reply` |
| a plaintext declaring `ask` whose `in_reply_to` names a message another agent of the recipient's deployment sent to this sender, under the same policy | not a reply: evaluated as `ask`, `quarantined` receipt |
| a valid plaintext the recipient's pair policy refuses under `shadow` | `stored` receipt, the would-be refusal audited, no quarantine item |
| a valid plaintext under `off` whose pair the recipient's rules would refuse | `stored` receipt, nothing evaluated |
| two arrivals of one logical message in step 4 at once, the recipient's policy changing between their evaluations | one commit, one disposition, and both journal entries carry it |
| a request body with an unknown member | `400 invalid_request` |
| a delivery whose recipient key epoch was replaced after admission | released, never handed out |
| a correctly signed request, arriving inside the step 4 window, whose timestamp is before the nonce-memory floor: signed before a relay restart and held in transit or re-sent unchanged, or signed after it by a clock running behind | `503 capacity_exhausted` with `retry_after_seconds` 60, no effect; the client re-signs, retries, and does not pause the channel |
| a replay, after a restart, of a request the service accepted before it, from a client whose clock ran up to 59 seconds ahead | refused (`503` at step 6, or `401` at step 4 once it is older than 300 seconds), no effect |
| a request from a client with an accurate clock, signed less than 60 seconds after nonce memory started at a relay restart | `503 capacity_exhausted` with `retry_after_seconds` 60, no effect; the same client's re-signed retry 61 seconds or more after nonce memory started, plus twice the clock bound where one applies, is served normally |
| a request whose timestamp is 60 seconds or more ahead of the service's clock | `401 unauthenticated` |
| a directory or nonce-memory read that fails during A.4 | `503 capacity_exhausted`, never `401` |
| a receipt-record, directory or transport-log read that fails during a submit, or a submit lock that cannot be taken | `503 capacity_exhausted`, no admission, no charge; never `envelope_conflict`, `contact_not_active`, `recipient_key_changed` or `insufficient_credit` |
| a receipt post during which a record read or write fails | `503 capacity_exhausted`, never `not_found`; the recipient's retry of the same bytes, while the admission is live or once the receipt committed, is recorded once and answers `200`; after a release with no receipt recorded it answers `not_found` |
| a submit to a recipient the sender has blocked, or who has blocked the sender | `403 contact_not_active`, the same status and body as the answer for a missing grant |
| two concurrent submits of one logical message naming different recipients | never both admitted; the one whose step 6 reads the other's admission is `envelope_conflict` |
| a block after a delivery's first hand-out, the recipient having stored it | the recipient's receipt is recorded and the sender reaches `recipient_stored` |
| a resubmit, after its receipt was recorded, carrying a different envelope for the same device and key epoch | `envelope_conflict` |
| a resubmit, after its admission was released without a receipt, carrying a different envelope for the same device and key epoch | `envelope_conflict`, no charge, nothing forwarded |
| a step 9 record that commits but whose outcome the service cannot confirm | `503 capacity_exhausted`, nothing forwarded, A.6.5 answers `unknown`; the client's resubmission of the same bytes is not charged again, and is admitted whenever a first submit of the same body would be, except that no credit is needed |
| a re-encryption after `recipient_key_changed` and the owner's confirmation, to a replacement device whose key epoch number equals the old device's, no receipt having been recorded for the first admission; the re-encryption being one that, as a first submit, would be admitted, credit aside | admitted, not charged again |
| a poll page whose first receipt fails verification | that message stays `pending`, the receipts after it are processed, and `receipts_after` advances past all of them |

The seeds above are `SHA-256` of the ASCII labels `khive-node-v1 vector sender kem`,
`khive-node-v1 vector recipient kem`, `khive-node-v1 vector sender sig`,
`khive-node-v1 vector recipient sig` and `khive-node-v1 vector ephemeral`; the nonce is the first 16
bytes of `SHA-256("khive-node-v1 vector nonce")`. They are published so the vectors can be
regenerated, and they are test values only.

### A.12 What this appendix does not change

Everything under "What stands" and "Compatibility obligation" above. In particular `comm.send`,
`comm.reply` and dual-write are untouched, no spoke listens, no credential is stored in a khive store,
and every existing channel adapter stays byte-identical.

<!-- deno-fmt-ignore-end -->
