# ADR-137: Tailnet Wire Transport for the khive Frame Protocol

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

The present daemon uses a Unix-domain socket at `~/.khive/khived.sock`, accepts length-prefixed JSON frames, and checks the local peer UID reported by the operating system, as of commit `e40e0392fba5ec7f9e4c7d406b68e35799f8087c`. Source: `crates/khive-runtime/src/daemon.rs:95-106,306-320,607-678,1443-1503,1608-1651` at commit `e40e0392f`.

No authenticated TCP or HTTP ingress for the khive verb surface exists in that source revision. The built-in foreground transport is MCP over stdio, and the transport registry registers `StdioTransport` only. Source: `crates/khive-mcp/src/transport.rs:34-90`; `crates/kkernel/src/cli.rs:259-283`; and `crates/khive-runtime/src/daemon.rs:95-106,1492-1503` at commit `e40e0392f`.

The existing daemon protocol accepts frame-carried `namespace`, `actor_id`, and `visible_namespaces` from the connecting process, while the Unix peer UID is the non-forgeable local fact. A remote transport must therefore establish identity at server ingress instead of trusting those frame fields. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at commit `e40e0392f`.

The current request surface has two execution paths after parsing: chain mode reaches `dispatch_op`, while single and parallel modes reach `dispatch_with_identity` separately. A policy check inserted in only one path would not constrain the other path. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at commit `e40e0392f`.

ADR-049 establishes the warm daemon, its Unix-socket framing, and its local client recovery behavior. Source: ADR-049, Decision sections 1 through 4 and Amendment 2. ADR-109 establishes the constrained-caller policy requirements, including a closed allowlist and fail-closed gateway behavior. Source: ADR-109, Decision and Hard rules.

## Decision

khive will add a second transport for the existing frame protocol. The transport is a TCP listener that may bind only to explicitly configured tailnet interface addresses. It must not bind loopback, wildcard, public, or other non-tailnet addresses; local processes continue to use the Unix-domain socket, so a loopback TCP path would duplicate an existing authenticated local path without adding one fact to authenticate it with.

### Shared protocol crate and handshake

A shared Rust crate, `khive-wire-protocol`, will define framing, the handshake, frame kinds, protocol versioning, and protocol errors. Its crate documentation is the normative wire specification. Rust clients will link this crate, while non-Rust clients will implement that specification directly.

The existing length-prefixed JSON framing remains the base framing. The first application frame on every TCP connection must be a `handshake` carrying a protocol version. The server answers with `handshake_ack` naming the accepted version, or with the wire error `unsupported_version` followed by connection close, before it accepts any request or subscription frame. The client must surface that rejection without fallback.

The protocol defines a closed set of frame kinds: `handshake`, `handshake_ack`, `request`, `response`, `error`, `cancel`, `subscribe`, `subscribe_ack`, `unsubscribe`, `unsubscribe_ack`, and `event`. The set is closed within a protocol version; adding a frame kind requires a protocol version bump. A subscription names exactly one topic, as defined under "Subscriptions, cursors, and replay" below, and the server rejects a subscription whose topic the caller's mapped peer class may not receive.

### Transport identity and actor mapping

Unix-domain-socket handling remains local. It continues to derive local peer facts from the operating system credential mechanism represented by the current `SO_PEERCRED` or equivalent path. Source: `crates/khive-runtime/src/daemon.rs:306-320,1608-1651` at commit `e40e0392f`.

For a TCP connection, the server must invoke `tailscale whois` against the connecting address and derive the peer identity from the stable node identity in that server-side result — the tailnet node identifier, not the transient network address alone. The server must then resolve that node identity through an explicit server-side mapping table. A mapping row assigns the node exactly one khive actor, one peer class, one write namespace, and one visible namespace set. Failure of the lookup, absence of a mapping, or an invalid mapping must reject the connection with `identity_rejected`. An unmapped peer must never receive a default actor.

One mapped node is one enforced trust boundary. The transport cannot distinguish two processes sharing a mapped node, so a node that hosts processes of differing privilege must not carry a privileged mapping. A constrained process on any node reaches khive only through the ADR-109 gateway or through the outer-ring edge adapter defined below, never through the native listener: every peer class explicitly declares whether it may open native-frame connections, an edge-only class does not, and the server rejects a native-frame connection from a node whose mapped class lacks native access, with `identity_rejected`. An edge-only class's callers are admitted only at the edge adapter surface defined under "Interop edge" below.

On the TCP transport, the request context is constructed only from the mapping row, with one defined lifecycle. At admission of each `request` frame, the server reads the connection's current mapping row and peer-class definition in one atomic read, mints the per-request identity token from exactly those fields — the mapped actor, write namespace, and visible namespace set — and evaluates the class allowlist from that same read. Every parsed operation in the frame executes under that single snapshot, so one frame is never split across two contexts, and a mapping or class change takes effect at the connection's next request frame, under the live-state rules in "Peer classes and dispatch enforcement" below. Peer-class policy composes with that context as two independent gates: the allowlist decides whether a parsed operation may execute at all, and the mapped context decides what data scope it executes against; neither is influenced by frame content. A frame-level attempt to supply `namespace`, `actor_id`, or `visible_namespaces` on this transport is rejected with the wire error `context_rejected` rather than ignored, so a client cannot silently believe an override took effect.

An operation-level `namespace` argument in the DSL is governed by a posture every peer class declares, and the posture of a class that declares nothing is the stricter one:

- A **constrained** class — the mandatory posture for every class carrying ADR-109 gateway traffic, and the default for an undeclared class — rejects every operation-level `namespace` argument with a per-operation authorization error at the DSL layer (ADR-016), and the operation executes against the server-substituted mapped namespaces. This is ADR-109's Hard rule 2 applied unmodified: a constrained caller's namespace is pinned server-side and is never caller-suppliable, by frame field or by DSL argument. Source: ADR-109, Hard rules, rule 2.
- A **scoped** class — a posture an operator declares explicitly, intended for first-party khive processes — treats the argument as a scope selector, never an identity override: it is honored only when it names the mapped write namespace (for a mutating operation) or a member of the mapped visible namespace set (for a read), and any other value fails that operation with a per-operation authorization error, never a silent retarget.

Both postures close the distinction between caller-supplied identity fields and operating-system-proven local credentials observed in the current daemon. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at commit `e40e0392f`.

The TCP transport does not add TLS inside the tailnet. This decision relies on WireGuard transport encryption supplied by the tailnet as a deployment precondition. The encryption guarantee is external to khive and is UNVERIFIED by the cited sources. Binding any non-tailnet address is out of scope for this ADR and requires a separate ADR with its own transport security decision.

### Peer classes and dispatch enforcement

The server must enforce a closed allowlist for every peer class at the dispatch chokepoint before execution. A class allowlist names permitted dispatch targets and permitted subscription topics. The dispatch-target type is a closed union of two members: a registered native verb, and a canonical pinned mounted-tool identifier as defined by the outbound-mount contract of the agentic process runtime proposal (ADR-142, a companion proposal under concurrent review). A mounted identifier participates in allowlist membership, class-write validation, and the allowlist comparisons below exactly as a verb does, while remaining unreachable through ordinary request or CLI dispatch — it is a dispatcher-internal tool target, and only the runtime's own agent-loop dispatcher can present one at this chokepoint. A remote owned-agent host reaches that dispatcher only through a server-owned record attachment: an agent-context frame names a server-issued, connection-scoped attachment reference, created only at `agent.spawn` or an owner `agent.resume` on that connection and binding the frame to exactly one process record, per the record-attachment contract of the agentic process runtime proposal (ADR-142, a companion proposal under concurrent review). An ordinary `request` frame names no attachment, carries no record context, and cannot reach a mounted identifier; a frame naming an unknown, expired, or other-connection attachment is denied, fail closed. The attachment selects a record and never asserts identity — the frame's actor remains solely the connection's server-side mapping. A class write validates every named target against that union and rejects an entry that names neither a registered verb nor a currently pinned mounted identifier. A read-only peer class is defined as a mapped class whose allowlist contains only explicit read targets and permitted topics; it must not include a state-mutating dispatch target or state-mutating protocol frame. For a native verb, the distinction is a closed registered **effect classification** — `read` or `mutating` — declared as a first-class field of the verb's handler registration; a verb whose registration carries no classification is treated as `mutating`. Speech-act metadata is expressly not this predicate: the existing verb-category tag is documented as not used for permission checking, and it can diverge from real effects — `db_diagnostics` registers as assertive while its diagnostic probe issues a real `PRAGMA wal_checkpoint(PASSIVE)` [`crates/khive-types/src/pack.rs:32-36,86-90` and `crates/khive-pack-kg/src/handler_defs.rs:964-972` at commit `e40e0392f`] — so read-only validation consumes only the registered effect classification, never a category, name, or description. For a mounted identifier, no schema or description can establish an external tool's side effects, so the distinction is a pinned, operator-declared effect classification — `read` or `mutating` — recorded per pinned identifier in the mount record at registration or re-pin, per the outbound-mount contract of the agentic process runtime proposal (ADR-142, a companion proposal under concurrent review). Class-write validation for a read-only class consumes the applicable classification for both target kinds and fails closed: a native verb or mounted identifier whose classification is not `read` — including one with no classification — is rejected from a read-only class at the class write, and the shared dispatch gate re-checks the classification of every target a read-only class presents at dispatch, denying a target no longer classified `read`. Examples, symmetric across the union: a read-only class write naming the native verb `stats` is accepted (registered effect classification `read`); the same write naming the native verb `update` is rejected (`mutating`); the same write naming pinned mounted identifier `search.web_fetch` is accepted only if that identifier's pinned effect classification is `read`, and rejected if it is `mutating` or absent. The implementation carries both arms as tests.

Both dispatch paths are a correctness condition, not an implementation preference. The enforcement must cover chain mode through `dispatch_op` and single or parallel mode through `dispatch_with_identity`; a check that covers only one path is non-conformant. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at commit `e40e0392f`.

The all-mode integration point must reject every parsed operation outside the mapped peer class before `run_parsed` dispatches it. This placement follows from the distinct current execution paths and is not an existing implementation. Source: `crates/khive-mcp/src/server.rs:2038-2122` and `crates/khive-mcp/src/server.rs:1031-1165` at commit `e40e0392f`.

The mapping table and class definitions are live server state, with three defined effect boundaries. First, request context: each `request` frame snapshots the connection's mapping and class at admission, as specified in "Transport identity and actor mapping", so revoking or narrowing a mapping takes effect for an already-open connection at its next request frame. Second, mapping deletion: deleting a node's mapping row terminates every open connection from that node at the change itself, with exactly one `identity_rejected` frame — eagerly rather than at the next operation, because a subscribed connection may never send another frame — and `identity_rejected` is the deletion's only terminal code, taking precedence over `subscription_revoked` even when the connection holds active subscriptions (the full abort semantics for in-flight operation ids are under "Operation correlation" and "Subscriptions"). Third, active subscriptions: when a mapping or class change short of deletion removes a topic from the allowed-topic set of a connection holding an active subscription to that topic, the server terminates that connection at the change with `subscription_revoked`; on reconnect, the client re-subscribes its remaining topics with its resume cursors, and a topic the narrowed class no longer permits is rejected per-operation with `subscription_denied`. A change that affects neither the connection's existence nor an active subscription — a verb-allowlist narrowing, for example — takes effect at the next request frame with no connection interruption.

The server can resolve any khive actor to its current mapped peer class: the class named by the current mapping rows naming that actor. An actor named by no current mapping resolves to no class, and every rule keyed on current class fails closed for it. Where more than one mapping row names the same actor — multiple mapped nodes acting as one seat — the actor's current class is defined only when every such row names the same class; otherwise resolution fails closed.

Peer classes have no ranked hierarchy. The only defined comparison between classes is the set relation of their allowlists: class A is at least as permissive as class B exactly when B's allowlist (dispatch targets and topics) is a subset of A's, and the effective permission of a pair of classes is the intersection of their allowlists, evaluated over the same closed dispatch-target union for every class. A rule that requires an ordering between incomparable classes fails closed. Downstream contracts that need "the more restrictive of two classes" evaluate it as this intersection.

### Remote endpoint behavior

Remote endpoint mode is explicit configuration, not an inferred recovery state. A client configured for a remote endpoint must not create a local runtime, fall back to local dispatch, or auto-spawn a daemon when the remote endpoint is unavailable, rejects a handshake, or cannot be reached.

This differs deliberately from ADR-049's local client behavior, which includes daemon recovery and defined local fallback classes. Source: ADR-049, Decision section 2 and Amendment 2. Remote endpoint failure is a surfaced failure because substituting a local runtime could create a divergent authority or data path.

### Interop edge: inner ring and outer ring

The transport surface has two rings. The **inner ring** is the native frame protocol defined above: the Unix-domain-socket and tailnet-TCP connections carrying the shared `khive-wire-protocol` framing, reserved for first-party khive processes and for constrained callers admitted under this ADR's identity and peer-class rules. Nothing outside this ADR's native framing is ever admitted to the inner ring, and internal khive processes use native frames or in-process dispatch exclusively.

The **outer ring** is a first-class, gated boundary surface, not a compatibility shim:

- **Inbound.** A remotely hosted agent or MCP-speaking client reaches khive only through a standard-protocol edge adapter, which may include a Streamable-HTTP-class MCP transport as a supported product surface where a deployment enables one. The edge adapter is an in-process transport module of the daemon, in the same structural position as the native TCP listener: it terminates the standard protocol and hands each admitted invocation to the shared dispatch chokepoint through a named in-process interface that carries only server-derived caller context. The adapter has no native-frame client path, and this ADR defines no out-of-process edge relay: a relay process would either need native-frame access an edge-only mapping denies, or would authenticate as the relay's node rather than the caller's — frame-carried identity being rejected on this transport — so that topology is rejected rather than left unspecified. The adapter binds only the same explicitly configured tailnet interface addresses the native listener may bind, and accepts only direct tailnet ingress: it derives the caller's identity exclusively, per request, from the direct connection's whois-resolved node identity under the same mapping rules as the native TCP transport, must not honor forwarded-identity or other client-supplied identity headers, and trusts no reverse proxy in front of it. Consistent with the MCP specification's current revision (2026-07-28), the adapter treats every request as self-contained: it holds no cross-request session state and infers no identity, capability, or protocol-version state from a prior request. A credential a client presents at this surface terminates at this surface: it is never forwarded upstream and never becomes an internal identity input — identity is solely the server-side node mapping — which applies the current MCP token-audience and token-passthrough prohibitions at this boundary rather than a khive-local preference. Subscription and event delivery are inner-ring native-frame capabilities and do not project through this adapter: MCP revision 2026-07-28 defines a client-opened `subscriptions/listen` channel for server-to-client notifications, but no generic, replayable projection of an arbitrary topic contract — the revision removes resumable SSE `Last-Event-ID` replay. This ADR deliberately does not map khive subscriptions or events onto that bounded notification surface; an eventing surface for standard-protocol clients would be a separate decision under the MCP specification's own notification mechanisms, and this ADR does not define one. A proxied, public, or otherwise non-tailnet HTTP exposure is out of scope for this ADR and requires the separate ADR named under "Non-tailnet binding" below. The per-peer-class allowlist enforcement defined in "Peer classes and dispatch enforcement" applies to every operation the edge adapter admits; a caller admitted at this surface is a mapped, edge-only peer class like any other caller, never an implicitly trusted internal client.
- **Outbound (deferred).** Mounting an external standard-protocol tool server as a tool source for runtime-owned agents is deliberately not defined by this transport ADR. Its registration, credential, attribution, and capability contract belongs to the agentic process runtime proposal (ADR-142, a companion proposal under concurrent review), which owns the dispatcher seam that outbound tool calls would share with ordinary verbs. Until that contract is accepted, no outbound mount is authorized by this ADR.

The inner/outer distinction is a hard boundary, stated once and applied everywhere in this ADR: a standard protocol such as MCP is never the internal fabric between first-party khive processes, and the inner ring's native framing is never exposed directly to a standard-protocol client. Enabling an inbound edge adapter for a given deployment is a configuration decision that must confirm the identity-mapping and peer-class-allowlist requirements above are satisfied before the adapter accepts a connection; it does not require a new ADR, but it does require the same server-side identity and allowlist mechanism this ADR defines for the native TCP transport.

## Protocol contract completeness

This ADR treats the framed protocol as a product contract, not a private daemon detail, and specifies the following wire-level mechanics normatively:

- **Canonical schema.** The `khive-wire-protocol` crate's documentation is the normative wire specification, and its schema is versioned together with the handshake's protocol version; a schema change that is not backward-compatible requires a version bump.
- **Capability negotiation.** The `handshake` frame carries the client's supported protocol version; the server answers `handshake_ack` or rejects with `unsupported_version` before accepting any other frame, as specified above.
- **Operation correlation.** Every `request`, `subscribe`, and `unsubscribe` frame carries a caller-generated operation id, unique across all three frame kinds for the lifetime of the connection; the server echoes that id on the operation's single terminal frame — `response` or `error` for a request, `subscribe_ack` or `error` for a subscribe, `unsubscribe_ack` or `error` for an unsubscribe — so a caller can correlate concurrent in-flight operations on one connection. A `cancel` frame references a `request` operation id only; a `cancel` naming a subscribe or unsubscribe id is treated as naming an unknown id, a no-op. The single-terminal-frame promise is scoped to operations a connection-terminal error does not interrupt: a connection-terminal error is the terminal outcome of the connection itself, it carries no operation id, and every operation id still unfinished when it is sent is aborted with the connection, with no individual echoed terminal — the one connection-terminal frame and the close are the only signal. A caller therefore treats every id unfinished at connection loss as **outcome-unknown**, on this path exactly as on a network drop: an unfinished `subscribe` or `unsubscribe` may be safely re-issued on a new connection (admission re-derives authorization, and unsubscribe of a non-subscribed topic is an idempotent no-op), while re-issuing an unfinished `request` is the caller's own idempotency decision, exactly as for any response the caller never observed.

### Wire error taxonomy

Wire-level errors are a closed set, distinct from verb-level DSL errors (ADR-016): a wire-level error is returned before or instead of DSL dispatch, while a DSL-level per-operation error (ADR-016's `ok: false` result) is a distinct, later failure mode inside a successful `response` frame. The complete wire error set for protocol version 1 is:

| Code                       | Condition                                                                                                                                                                                     | Terminal scope |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------- |
| `unsupported_version`      | Handshake names no mutually supported protocol version.                                                                                                                                       | Connection     |
| `identity_rejected`        | Whois lookup failed, no or invalid node mapping, or native-frame ingress attempted by a class without native access.                                                                          | Connection     |
| `malformed_frame`          | A frame cannot be decoded or violates the frame grammar.                                                                                                                                      | Connection     |
| `frame_too_large`          | A frame exceeds the configured maximum size.                                                                                                                                                  | Connection     |
| `subscriber_overflow`      | The connection's bounded outbound event queue reached its limit.                                                                                                                              | Connection     |
| `subscription_revoked`     | A live mapping or peer-class change short of mapping deletion removed authorization for a topic with an active subscription on this connection; a deletion sends `identity_rejected` instead. | Connection     |
| `context_rejected`         | A frame-level attempt to supply `namespace`, `actor_id`, or `visible_namespaces` on a mapped transport.                                                                                       | Request        |
| `peer_class_denied`        | A parsed operation names a verb outside the mapped class allowlist.                                                                                                                           | Request        |
| `subscription_denied`      | A `subscribe` names a topic outside the mapped class's allowed-topic set.                                                                                                                     | Request        |
| `already_subscribed`       | A `subscribe` names a topic that already has an active subscription on this connection.                                                                                                       | Request        |
| `cursor_expired`           | A `subscribe` resume cursor is older than the topic's retention window.                                                                                                                       | Request        |
| `in_flight_limit_exceeded` | A `request` arrives while the per-connection in-flight limit is reached.                                                                                                                      | Request        |
| `deadline_exceeded`        | The server abandoned the request at its deadline.                                                                                                                                             | Request        |
| `cancelled`                | The request was terminated by a `cancel` frame before its normal completion.                                                                                                                  | Request        |
| `shutting_down`            | The server is draining and refuses new work.                                                                                                                                                  | Request        |
| `internal`                 | An unclassified server-side transport failure.                                                                                                                                                | Request        |

A connection-terminal error is followed by connection close; a request-terminal error terminates only the operation id it echoes — a request, subscribe, or unsubscribe id — and the connection stays usable. The set is closed within a protocol version: adding a code requires a protocol version bump, and a client that receives a code it does not recognize must treat it as `internal` — request-terminal, retriable only under the caller's own policy — rather than inventing semantics for it.

> **Amended.** Amendment 1, decision 5, supersedes the unrecognized-code sentence above for a code
> that arrives without an operation id. The rule as written holds for an id-bearing unknown code
> and is wrong for an id-less one, because a request-terminal outcome needs a request to terminate.

### Limits and backpressure

Every deployment configures a maximum frame size and a maximum number of in-flight requests per connection. A frame exceeding the configured size is rejected with `frame_too_large` and the connection is closed rather than partially buffered. A request arriving at the in-flight limit is rejected with `in_flight_limit_exceeded`; the connection stays open and the caller may retry after an earlier request completes. Each connection's outbound event queue is bounded; reaching that bound disconnects the subscriber with `subscriber_overflow` rather than growing server memory without limit.

### Deadlines and cancellation

A `request` frame may carry an optional `deadline_ms`: a positive integer number of milliseconds measured from server receipt of the frame, evaluated against the server's monotonic clock. No wall-clock synchronization between client and server is assumed. The deadline's scope is the entire request frame — the whole DSL batch or chain it carries.

A `cancel` frame references a request id and asks the server to terminate that request. Deadline expiry and cancellation take effect at operation boundaries: the server never aborts an individual parsed operation mid-execution; it stops before starting the next operation, and remaining operations do not start.

Exactly one terminal frame is sent per request id. A cancelled request terminates with either its normal `response` (completion won the race) or an `error` carrying `cancelled`; a request that misses its deadline terminates with `deadline_exceeded`. A `cancel` naming an unknown or already-terminal request id is a no-op. There is no separate cancellation-acknowledgement frame; the request's single terminal frame is the acknowledgement.

For a request containing mutating operations, `deadline_exceeded` and `cancelled` make no claim about effect commitment: operations that completed before the termination point remain committed, and their per-operation results are not delivered. A caller must treat these two errors as outcome-unknown and verify by read before retrying a non-idempotent write. Protocol version 1 deliberately provides no cross-connection idempotency-key mechanism; a deployment that needs exactly-once retry semantics builds them at the verb layer, not the transport.

### Subscriptions, cursors, and replay

This ADR, not a downstream crate or client contract, owns the subscription topic namespace and the state-change payload envelope. A topic is a closed, versioned string of the form `<domain>.<event>` (for example `comm.message_created`). The authorization unit is the topic: a peer class's allowlist enumerates the topics it may receive, a `subscribe` frame names exactly one topic, and the server rejects the subscription with `subscription_denied` before any event flows if the topic is not in the caller's allowed set.

At most one subscription per `(connection, topic)` pair is active at any time; subscription control operations carry the caller-generated operation ids defined under "Operation correlation", and each terminates in exactly one `subscribe_ack`, `unsubscribe_ack`, or `error` frame echoing its id, under that section's connection-terminal scoping — a control operation aborted by a connection-terminal error receives no individual echo.

Subscription admission is linearized with authorization changes. A `subscribe` takes the same one-atomic-read mapping/class snapshot a `request` frame takes at admission, and its activation — the point it becomes eligible for event delivery, marked by its `subscribe_ack` — is serialized with mapping and class updates under a single ordering: the server revalidates the topic against the connection's then-current class immediately before sending `subscribe_ack`, under that same ordering. Every `subscribe` therefore either observes an authorization change and terminates with `subscription_denied`, or activates before the change and is then governed by the change's revocation rule in "Peer classes and dispatch enforcement"; no interleaving can leave active a subscription the current class does not authorize. A mapping deletion is the exclusive connection-terminal exception, and it wins over everything on the connection: the server sends exactly one `identity_rejected` frame and closes — `identity_rejected` takes precedence over `subscription_revoked` when the deleted mapping's connection also holds active subscriptions, so a deletion never produces two connection-terminal frames or a code choice — and every pending `request`, `subscribe`, and `unsubscribe` id aborts with the connection under the outcome-unknown rule in "Operation correlation", with no individual echoed terminals. `subscription_revoked` remains the connection-terminal code for the narrower event: a mapping or class **change**, short of deletion, that removes an actively subscribed topic.

- `subscribe { id, topic, resume_cursor? }` opens delivery for one topic on the connection. With `resume_cursor` absent, delivery starts at new events only. With `resume_cursor` present, the server replays every retained event with a cursor greater than `resume_cursor`, in order, before delivering new events; a `resume_cursor` older than the topic's retention window is rejected with `cursor_expired` rather than silently resuming from an arbitrary point. A `subscribe` naming a topic that already has an active subscription on the connection is rejected with `already_subscribed`; the existing subscription and its delivery position are unchanged.
- `subscribe_ack { id, topic, start_cursor }` confirms the subscription and names the cursor position delivery begins after. A denied or failed subscribe instead terminates with an `error` echoing the subscribe id — `subscription_denied`, `cursor_expired`, or another request-terminal code.
- `event { topic, cursor, occurred_at, payload }` delivers one state change: `cursor` is the server-assigned, per-topic, strictly increasing resumption cursor; `occurred_at` is the server-assigned event time; `payload` is a topic-specific JSON object whose exact field-by-field shape this ADR delegates, by name, to the implementation-phase topic catalog below. `event` frames carry no operation id; they are correlated by topic and ordered by cursor.
- `unsubscribe { id, topic }` ends delivery; the server confirms with `unsubscribe_ack { id, topic }`, after which no further `event` for that topic arrives on that connection. An `unsubscribe` naming a topic with no active subscription succeeds with its `unsubscribe_ack` — an idempotent no-op, mirroring the `cancel` rule for unknown request ids.

A server-initiated end of delivery is not signalled per topic: when live mapping or class state removes authorization for an actively subscribed topic, the connection terminates with `subscription_revoked` as specified in "Peer classes and dispatch enforcement", and recovery is reconnect plus re-subscribe with resume cursors.

Cursor scope is per topic; a cursor from one topic has no meaning for another. The server persists a bounded per-topic retention log and holds no per-subscriber acknowledgement state: the client owns its resume position (the cursor of the last event it processed) and presents it on reconnect. Within one connection, events for one topic are delivered in strictly increasing cursor order with no gaps inside the retention window. Across reconnects, delivery is at-least-once: a client that reconnects with an older resume position receives replayed events again and deduplicates by `(topic, cursor)`.

### Compatibility policy

Protocol version numbers are monotonic; a server supports the current version and at least the immediately prior version, and a breaking wire change — including a new frame kind or a new wire error code — is never introduced within a version number.

> **Amended.** Amendment 1, decision 1, bounds the rule above at the version floor: the supported
> range is `[max(1, current - 1), current]`, so at the current version of `1` there is no prior
> version and the range is `[1, 1]`. Read without that bound, the sentence would require supporting
> version `0`, which decision 1 declares invalid.

### Implementation-phase deliverables

The exact numeric defaults for frame-size and in-flight-request limits, a golden-frame fixture suite, decoder fuzzing, cross-client conformance testing, and the exhaustive per-topic subscription payload catalog (the concrete list of topics and each topic's `payload` field schema, within the envelope this ADR defines above) are implementation-phase deliverables owned by the `khive-wire-protocol` crate maintainer. Enabling TCP subscriptions, or any inbound edge adapter, for any peer class is gated on that conformance suite passing, including conformance coverage of the published topic catalog; this ADR does not authorize enabling subscriptions before the gate is satisfied.

> **Amended.** Amendment 1, decision 2, fixes the frame-size default at 8 MiB, so frame-size is no
> longer among the deliverables listed above. The in-flight-request limit named in the same sentence
> is unaffected and remains an implementation-phase deliverable.

### MCP placement

MCP over stdio remains a boundary compatibility adapter for foreign agent runtimes reached through the outer ring defined above. It is not the internal fabric. An MCP adapter that reaches a remote endpoint uses this ADR's native frame protocol rather than defining a competing internal protocol.

## Supersession and amendment of ADR-109

This ADR amends one specific part of ADR-109: the section "Fork (a): Process boundary" and its "Resolution (Open Question 1 - process boundary)" paragraph, which selects a thin gateway binary that "connects to the warm daemon as a client, a proxy" without specifying the transport that connection uses. This ADR supplies that transport: a tailnet-connected gateway binary connects to the daemon over this ADR's native frame protocol, with the daemon performing server-side peer-identity mapping and peer-class allowlist enforcement at the shared dispatch chokepoint, rather than through an MCP stdio or HTTP-facing remote executor. Source: ADR-109, "Fork (a): Process boundary" and its Resolution.

ADR-109's "Hard rules (not forked)" 1 (closed, explicit verb allowlist), 2 (pinned, non-caller-suppliable namespace), and 6 (fail-closed on anything outside the contract) remain in force unchanged for a peer class defined by this ADR; this ADR's server-side peer-class allowlist is an additional transport-layer enforcement point, not a replacement for those rules. Rule 2 is carried concretely by the **constrained** namespace posture defined in "Transport identity and actor mapping": every class carrying ADR-109 gateway traffic is constrained, so its callers' operation-level `namespace` arguments are rejected and the server substitutes the mapped namespaces; the **scoped** posture exists only for operator-declared first-party classes outside ADR-109's constrained-caller scope. Source: ADR-109, "Hard rules (not forked)", rules 1, 2, and 6. ADR-109's separate gateway binary also remains the structural boundary for constrained processes co-resident with higher-privilege clients; this ADR's one-node-one-boundary rule above is what keeps the native listener from dissolving that boundary. Source: ADR-109, "Fork (a): Process boundary", alternative A1.

This amendment is limited to the gateway's process-boundary transport, ingress identity, and the corresponding dispatch integration. ADR-109 remains the authority for its capability declaration format, authentication resolution, and Phase B relationship (Forks (b), (c), and (d)), which this ADR does not amend. Source: ADR-109, "Decision" and "Resolutions". A future revision of ADR-109 should add a forward reference to this ADR under its "Fork (a): Process boundary" section, naming this ADR as the source of the process-boundary transport it left open.

## Amendment 1: Wire-contract closure before the first consumer

- **Status:** Proposed
- **Date:** 2026-08-22

### Scope and precedence

The wire crate `khive-wire-protocol` implements framing, envelope grammar, and handshake
sequencing decisions that this ADR left unstated or stated only in prose. Each such decision is
an interoperability commitment: a second implementation written from the ADR alone would have no
way to reproduce it, and would diverge silently rather than fail loudly. This amendment closes
that gap by promoting each decision to specified behaviour, and, where the implemented behaviour
is wrong for the contract, by specifying the correct behaviour and requiring the code to move.

This amendment closes only the wire-level ambiguities enumerated below. It does not authorize a
transport server, an identity mapping store, a deadline executor, or any other feature the parent
ADR defers; those remain separately tracked. It precedes the first consumer of the crate: at the
time of writing the crate has no in-tree consumer, so every behaviour named here can still be
changed without a compatibility break.

Each decision below is marked **RATIFIED** where the implemented behaviour is correct and this
amendment records it as contract, or **CHANGED** where the contract differs from what the crate
does today and the crate must move to match.

### Framing and version

1. **Initial version and floor — RATIFIED.** The initial protocol version is `1`. Version `0` is
   not a valid protocol version. Initial supported range is `[1, 1]`. A handshake naming a version
   outside the receiver's supported range is rejected rather than negotiated downward.

   **The rejection layer is stated normatively because two layers could plausibly own it.** A
   version field carrying any non-negative integer **representable as an unsigned 32-bit value**,
   `0` included, is syntactically well-formed and MUST decode: the decoder's job is grammar, and a
   receiver that rejected `0` as `malformed_frame` would close the connection on a frame it could
   have answered. Admission is where the range is enforced: a handshake naming a version outside
   the receiver's supported range, `0` and `99` alike, is rejected at handshake admission with
   `unsupported_version`. Version `0` is therefore not special-cased on the receive path; it is out
   of range like any other unsupported value, and the floor of `1` is what makes it out of range.

   **The wire type is a 32-bit unsigned integer, and that bound is normative rather than
   incidental.** A JSON integer too large for it, `4294967296` being the boundary case, is a
   grammar violation and is rejected at decode as `malformed_frame`; it never reaches handshake
   admission and never produces `unsupported_version`. Saying so is load-bearing: without the
   bound, "any non-negative integer MUST decode" reads as arbitrary precision, and an
   implementation built on a bignum type would admit `4294967296` and reject it at admission while
   this one rejects it at decode. That is a disagreement at exactly the layer this decision
   settles, so the overflow boundary is a required conformance vector rather than an edge case.

   Locally constructing a version-`0` frame is a separate matter from receiving one. An encoder
   MUST NOT emit a handshake or handshake acknowledgment naming version `0`; that is a local
   construction error, not a wire error, and it never reaches a peer. Requiring it keeps a
   conforming encoder from producing a frame whose only possible outcome at the far end is
   rejection.

   **This decision bounds the parent's compatibility rule at the floor, and the bound has to be
   written down because the code already has it and the prose does not.** The parent states that a
   server supports the current version and at least the immediately prior version. Read literally at
   a current version of `1`, that sentence requires supporting version `0`, which the same decision
   declares invalid. The supported range is therefore `[max(1, current - 1), current]`: the lower
   bound saturates at the floor rather than descending below it, so at version `1` the range is
   `[1, 1]` and there is no prior version to support. The parent sentence carries an inline note
   pointing here, and the crate documentation restates the unbounded form in its own versioning
   section, which the precedence list below names.

   This is the quietest kind of divergence and worth stating for that reason. The implementation
   already saturates correctly, so nothing fails and no test goes red; only the two prose statements
   disagree with the decision, and an independent implementer working from either of them would
   build a receiver that accepts a version the protocol does not define.

2. **Frame ceiling and accounting — RATIFIED. This decision fixes the frame-size default that the
   parent left deferred.** Frames are length-prefixed with a four-byte big-endian payload length.
   The configurable size limit counts the serialized bytes of the JSON payload only and excludes the
   four-byte prefix. The default limit is 8 MiB. An independent absolute ceiling of `u32::MAX`
   bounds the prefix itself and is not configurable upward.

   The parent lists "the exact numeric defaults for frame-size and in-flight-request limits" among
   its implementation-phase deliverables, and its Consequences section repeats that "the numeric
   limit defaults remain implementation-phase deliverables". Ratifying 8 MiB settles the first of
   those and leaves the second standing: the in-flight-request limit is still an implementation-phase
   deliverable, and this amendment names no value for it. Separating them is load-bearing rather
   than tidy. A default left deferred is a default two implementations may choose differently, and a
   frame that one accepts and the other rejects is an interoperability break with no wire error to
   explain it. Both parent sentences carry an inline note pointing here.

### Envelope grammar

3. **Operation identifiers — RATIFIED.** An operation id carried on the wire is a non-empty
   string; the empty string is rejected at decode. The crate MAY retain unrestricted in-memory
   constructors for test and internal use, provided a frame carrying an empty id is never
   accepted from the wire.

4. **Envelope member semantics — CHANGED.** The typed frame envelope rejects unknown members,
   rejects duplicate members, and treats omission as the only representation of an absent optional
   envelope field; an explicit `null` is rejected rather than read as absence. The crate currently
   allows duplicate members to collapse under last-wins before field validation runs, which makes
   acceptance depend on member order: two documents differing only in the order of two duplicate
   `id` values can classify differently. Duplicate rejection must therefore happen before field
   validation, so that decision 3 is deterministic across JSON parsers.

   This rule governs the envelope only. The contents of opaque subvalues are exempt, per
   decision 9.

### Error and sequence semantics

5. **Unknown error codes — CHANGED. This decision supersedes the parent's unknown-code rule.**
   The parent states under "Wire errors" that a client receiving a code it does not recognize
   "must treat it as `internal` — request-terminal, retriable only under the caller's own policy".
   That rule is correct for an unknown code that carries an operation id and wrong for one that
   does not, because a request-terminal outcome needs a request to terminate. Where this decision
   and the parent paragraph disagree, this amendment governs.

   Both cases are decided at the decode boundary, so that acceptance never depends on what a
   consumer does afterwards:

   - **Unknown code WITH an operation id** — surfaced through a fallback that preserves the raw
     code string for diagnostics, request-terminal against that id, connection stays usable. The
     fallback frame is not re-encodable: it may be inspected but never relayed onward as though it
     were understood. This is the parent's rule, kept.
   - **Unknown code WITHOUT an operation id** — `malformed_frame`, connection-terminal, following
     decision 6's id-less path. The parent's `internal` classification is superseded here.

   The parent's "Wire errors" paragraph carries an inline note pointing here, so a reader who
   lands on that paragraph and stops there does not walk away with the superseded rule.

6. **Handshake sequence violations — CHANGED, and the change is one of role coverage rather than
   of the rule.** A frame arriving before the handshake completes, a handshake frame arriving after
   the handshake completes, and a frame arriving in the wrong direction are each `malformed_frame`
   and each terminate the connection permanently. These are connection-terminal and carry no
   operation id, because no operation is established. That rule is the parent's and it stands
   unaltered.

   **What must move is that the crate enforces it for one endpoint role only.** The gate is a
   server-side inbound gate: it admits the client-to-server kinds and treats a server-to-client kind
   as a direction violation, which is correct for a server and is the exact mirror of what a client
   needs. A client built on this crate has no gate at all, so a `request` frame arriving at a client
   — a direction violation by the same rule — is rejected by nothing. The decode path cannot cover
   the gap, because it is deliberately role-agnostic and state-agnostic: it reads the length prefix
   and decodes the payload without a direction argument or a handshake state, and it is correct for
   it to stay that way.

   A conforming implementation MUST therefore enforce the sequence and direction rules on both
   endpoint roles, with the admissible frame-kind set mirrored per role. Labelling this RATIFIED
   would have been the more comfortable reading and it would have been false in a way that matters:
   the implementation fence below turns on the CHANGED set being exactly right, and the conformance
   matrix requires each vector to name the endpoint direction it is decoded at. A matrix with a
   direction field, run against a crate that gates one direction, cannot execute half its own rows.

### Server-produced fields

7. **Topic validation boundary — RATIFIED.** The codec accepts any string in a topic position.
   Catalog authorization is enforced at the server boundary, not in the codec. Codec permissiveness
   is not permission to produce arbitrary topics: a conforming server emits only catalog-valid
   topics. Both halves are stated because acceptance of a string must not be mistaken for licence
   to send one.

8. **Event timestamp boundary — RATIFIED.** The codec accepts any string in `occurred_at`. A
   conforming server emits RFC 3339. As with decision 7, decoder permissiveness and producer
   obligation are separate, and an independent implementation must not infer the second from the
   first.

### Opaque payload fidelity

9. **Opaque subvalue preservation — CHANGED.** The contents of `response.result` and
   `event.payload` are opaque to this crate. The crate currently documents and tests semantic
   rather than lexical preservation, so a value that is decoded and re-encoded can come out with
   members reordered and a number's lexeme changed. That is acceptable for an endpoint that parses
   the value and wrong for anything that forwards it, and the transport's default must serve the
   forwarding case, because the fields are declared opaque and no consumer yet constrains them.

   **The conformance operation is decode-then-encode over a single frame, and nothing else.** For
   a frame decoded from bytes and re-encoded without the holder modifying that subvalue, the bytes
   of `response.result` and `event.payload` in the output must be identical to the bytes occupying
   that subvalue's span in the input. Stating the operation is load-bearing: this crate performs no
   I/O and exposes no "relay", so a requirement phrased against relaying names an operation that
   does not exist and cannot be tested. Decode-then-encode is the only operation the crate has, and
   it is the one a forwarding consumer composes.

   **The span is the complete raw byte range of the value, and every byte in it is preserved.** It
   begins at the first byte of the value's first token and ends at the last byte of its last token.
   Interior whitespace is inside the span and is preserved with everything else; no normalization
   is permitted anywhere within it. An earlier draft of this decision allowed an implementation to
   normalize interior whitespace, which contradicted the byte-exactness it was defining: two
   implementations could then produce different output from one input and both claim conformance,
   which is the disagreement the decision exists to remove. Preservation is also the cheaper rule
   to implement, since an implementation satisfying this decision already holds the raw bytes and
   normalizing would be extra work.

   Consequences of that span, each stated because it is otherwise implementation-defined:

   - **Preserved,** as part of the raw span: member order, the original lexical form of numbers,
     string escape sequences as written, interior whitespace, and any duplicate members nested
     _inside_ the opaque subvalue. Duplicate rejection under decision 4 governs the typed envelope
     only, and must not be applied recursively into an opaque subvalue.
   - **Outside the span, and therefore not governed here:** whitespace before the value's first
     token or after its last, which belongs to the enclosing envelope; envelope formatting
     generally; and any value the holder constructed locally rather than receiving, which has no
     source bytes to preserve and is encoded by the ordinary serializer.

   **This requires a representation the current typed structs cannot provide.** `serde_json::Value`
   discards member order and number lexemes at parse time, so satisfying this decision means
   carrying the received subvalue as its source bytes (for example a raw-value representation
   retained alongside, or in place of, the parsed view). An implementation MAY additionally expose
   a parsed convenience view; it MAY NOT make the parsed view the only representation. This is the
   concrete work decision 9 fences.

   This requirement does not make envelope formatting byte-stable and does not require whole-frame
   verbatim forwarding.

### Payload shape

10. **`event.payload` must be a JSON object — CHANGED.** The parent ADR specifies `payload` as "a
    topic-specific JSON object whose exact field-by-field shape this ADR delegates, by name, to
    the implementation-phase topic catalog below". The crate types the field as an arbitrary JSON
    value, so a scalar, array, or `null` payload is accepted on the wire while an implementation
    written from the ADR would reject it. The object requirement is retained: "field-by-field"
    presupposes fields, and an object is the only shape a topic catalog can extend compatibly,
    whereas a scalar payload cannot gain a field without a breaking change.

    **The requirement binds both directions, and saying only "at decode" would leave the contract
    open on the side that matters more.** A decoder MUST reject a non-object `event.payload`. An
    encoder MUST also reject one, as a local construction error in the manner of decision 1's
    version-`0` clause, so a conforming implementation cannot put a frame on the wire that a
    conforming peer must refuse. The crate currently enforces neither: the encode-side validator
    matches `Frame::Event` and performs no shape check, so `encode_frame` will emit a scalar,
    array, or `null` payload today. A decode-only rule would leave exactly that behaviour legal,
    and the resulting split — one implementation sending what another must reject — is the
    interoperability failure this amendment exists to prevent. Both directions therefore need
    conformance vectors.

    This item was found while confirming the others and is recorded here rather than deferred,
    because an amendment that claims to close the wire contract while leaving a known frame-shape
    divergence open would misrepresent its own completeness.

### Precedence over the crate documentation

The parent states that the crate's own documentation "is the normative wire specification", which
makes that documentation a second authority and not merely a description. Six of the ten decisions
above contradict it as it stands today — decisions 1, 4, 5, 6, 9, and 10 — so a second implementer
reading the crate docs, exactly as the parent instructs, would build behaviour this amendment
removes. **Where this amendment and the current crate documentation disagree, this amendment
governs, and the documentation is superseded in the following places:**

- The versioning section restates the parent's compatibility rule in its unbounded form
  (`crates/khive-wire-protocol/src/lib.rs`) — superseded by decision 1.
- The module documentation's stated boundaries on optional-null equivalence and duplicate members
  (`crates/khive-wire-protocol/src/lib.rs`) — superseded by decision 4.
- The module documentation's unknown-error-code paragraphs (`crates/khive-wire-protocol/src/lib.rs`)
  and the wire-error module's own documentation, which quotes the parent's superseded sentence
  verbatim and presents it as the governing rule (`crates/khive-wire-protocol/src/error.rs`) —
  superseded by decision 5. The displacement is bounded: for the id-less case only, per that
  decision's own split.
- The handshake gate's documentation, which presents a server-side-only gate as the implementation
  of the sequence rule (`crates/khive-wire-protocol/src/lib.rs`) and the handshake module's own
  scope statement (`crates/khive-wire-protocol/src/handshake.rs`) — superseded by decision 6. The
  displacement is bounded: the rule is unaltered; what is displaced is its scoping to one endpoint
  role.
- The module documentation's opaque-payload-fidelity section, together with the named codec test it
  cites as pinning that behaviour (`crates/khive-wire-protocol/src/lib.rs`) — superseded by decision
  9.
- The characterization of `event.payload` as data whose shape the crate does not own, as it applies
  to the outermost type only (`crates/khive-wire-protocol/src/lib.rs`) — superseded by decision 10.
  The displacement is bounded: the per-field shape of a topic's payload remains the catalog's
  business; only the outermost type is displaced.

Three of those entries are bounded rather than wholesale — decisions 5, 6, and 10 — and in each case
the bound is the substance. Decision 10's is the one worth spelling out, because it displaces a
disclaimer rather than a statement: the crate documentation never says a non-object payload is
acceptable, it says the shape of `event.payload` is not the crate's business. That is not a weaker
form of the same thing. A document that disclaims ownership of a rule licenses the absence of that
rule just as effectively as one that states the permissive version, and the reader who follows the
disclaimer arrives at the same wrong implementation.

**This list is a debt, not a resolution.** The implementation that ratifies these decisions updates
the crate documentation in the same change, at which point this section becomes redundant and
should be deleted rather than left standing as a permanent second source of truth.

### Implementation fences

- An implementation MAY retain unrestricted in-memory constructors, parsed convenience views over
  opaque values, and server-side topic and timestamp validation, provided the wire behaviour
  specified above remains observable from outside.
- An implementation MAY NOT add frame kinds or error codes under this amendment, and MAY NOT fold
  the separately tracked server, identity-mapping, or deadline work into it.
- **No first consumer of `khive-wire-protocol` may merge while any decision marked CHANGED above
  still implements the superseded behaviour.** Those are decisions 4, 5, 6, 9, and 10 — five of the
  ten.
  A consumer merged against the old behaviour converts each of them from a free correction into a
  compatibility break, which is precisely the cost this amendment exists to avoid. Decision 6 is in
  that set for a different reason than the others: its rule is not changing, its role coverage is,
  and a consumer that ships a client without the mirrored gate is as hard to correct afterwards as
  one that ships the wrong rule.

### Conformance

**This is a requirement on ratification, not a description of what exists.** No vector matrix is
checked in today. `crates/khive-wire-protocol/tests/` holds eleven golden hex fixtures, one
well-formed frame per kind, and `golden_frames.rs`, which exercises those fixtures and a set of
malformed-frame cases. That is a useful regression floor and it is not a conformance matrix: it
carries no negative vectors for the rules this amendment changes, no independent implementation,
and no agreement criterion between implementations. Saying so plainly is the point of this
section. An amendment that asserted its own verification in the present tense would hand a reader
an unlocatable artifact and make every normative claim above unreproducible.

**This amendment MUST NOT be ratified until the matrix is checked in.** The artifact is a set of
vectors under `crates/khive-wire-protocol/tests/`, plus a runner that executes them against an
implementation.

**A vector is not a byte string paired with an outcome, because the rules above are stateful and
they do not all live at the same stage.** Whether a `handshake` frame is legal depends on whether
the handshake has already completed, and whether a `response` frame is legal depends on which end
received it — the same bytes classify differently in each case. Two rules above are also split
across stages on purpose: decision 1 requires version `0` to decode successfully and then be
rejected at handshake admission, and decisions 1 and 10 both require an encoder to refuse a frame
that has no wire representation at all. A schema with one outcome field cannot express either.

**Every vector therefore names the stage it exercises**, one of `decode`, `admit`, or `encode`, and
carries the fields that stage has:

- **Stage `decode`** — endpoint direction, the input bytes, and an expected outcome of _accepted_
  (yielding a frame) or _rejected_ with a named wire error code and its terminal scope. Grammar
  only: a value the grammar admits is accepted here even when a later stage will refuse it.
- **Stage `admit`** — endpoint direction, an ordered sequence of preceding frames or an explicit
  statement of the gate state the vector begins from, the already-decoded frame under test, and an
  expected outcome of _admitted_ or _rejected_ with a named wire error code and its terminal scope.
  A one-frame vector states "fresh connection, handshake not yet completed".
- **Stage `encode`** — the locally constructed frame under test and an expected outcome of _emitted_
  (yielding bytes) or _refused as a local construction error_. A refusal at this stage carries no
  wire error code and no terminal scope, because nothing reached a wire and no connection exists to
  terminate. A vector that demanded one of an encode case would be unsatisfiable by construction.

**A rule split across stages is covered by one vector per stage, and both are required.** Version
`0` is the worked example: a `decode` vector asserting _accepted_, and an `admit` vector on the
resulting frame asserting _rejected, `unsupported_version`, connection-terminal_. Either vector
alone proves the opposite of decision 1 from the other stage's point of view, which is exactly the
disagreement the decision was written to settle.

Required coverage, positive and negative, is every rule above, each at the stage or stages that
rule lives at: boundary frame lengths on both sides of the limit; version zero at both `decode` and
`admit` per the split above, out-of-range versions at `admit`, and the `4294967296` overflow
boundary from decision 1 at `decode`; empty operation ids; unknown and duplicate and
explicitly-null envelope members; both id-bearing and id-less unknown error codes; each of the
three handshake sequence violations from decision 6, **at `admit` and once per endpoint role**,
since that decision's whole change is role coverage; arbitrary topic and timestamp strings; opaque
subvalues that round-trip byte-exactly; a version-`0` handshake at `encode`, expected refused as a
local construction error per decision 1; and non-object event payloads at both `decode` and
`encode` per decision 10, the second expected refused as a local construction error.

**Consumed frame length is an agreement criterion for accepted frames only.** A decode error in
this crate deliberately carries no consumed count: once a frame fails to decode the stream
position is unrecoverable, the error is connection-terminal, and a transport is required to close
rather than resynchronize. Asking two implementations to agree on where a rejected frame ended
would demand a value one of them is designed not to produce, which would make the gate
unexecutable rather than strict. For rejected frames the agreement criteria are the
classification and the terminal scope; the length criterion does not apply.

Vectors MUST be run through the Rust crate and through at least one independent JSON
implementation, which must agree on:

- the outcome of every vector, in that vector's stage vocabulary: accepted or rejected at `decode`,
  admitted or rejected at `admit`, emitted or refused at `encode`;
- the wire error code and terminal scope of each `decode` or `admit` rejection. An `encode` refusal
  has neither, and the two implementations are required to agree only that the frame was refused —
  how each reports a local construction error is its own business, and demanding a shared spelling
  for it would export this crate's error type into the contract;
- consumed frame length, for accepted `decode` vectors only, per the paragraph above;
- byte-exact opaque-subvalue round trip under the decode-then-encode operation defined in
  decision 9.

The independent implementation is what makes the matrix a conformance artifact rather than a
second copy of this crate's own assumptions, and it is why decision 9 had to name a concrete
operation: a second implementation cannot be tested against a requirement phrased as an intention.

## Consequences

The daemon gains a location-transparent wire transport while retaining one framing and dispatch contract. The shared crate makes version and frame-kind changes reviewable as protocol changes instead of client-specific conventions.

The server becomes responsible for remote identity proof, actor mapping, and peer-class admission. This increases operational responsibility for the mapping table and for availability of `tailscale whois`, but it prevents a remote caller from selecting an actor through frame content, and the one-node-one-boundary rule makes node placement of processes an explicit deployment decision rather than an implicit trust grant.

Subscription delivery becomes part of the same authenticated connection as requests, using the topic namespace, payload envelope, ordering guarantee, and authorization hook this ADR defines in "Protocol contract completeness." Only the exhaustive per-topic payload catalog and the numeric limit defaults remain implementation-phase deliverables, gated on the conformance suite as specified there.

> **Amended.** Amendment 1, decision 2, fixes the frame-size default at 8 MiB. "The numeric limit
> defaults" in the sentence above now refers to the in-flight-request limit alone.

Remote clients receive explicit endpoint failures rather than implicit local recovery. Deployments must therefore provision an endpoint selector, connection diagnostics, and operator remediation for unavailable remote services.

No general public-network listener is introduced. A deployment that needs a non-tailnet listener must first define authentication, encryption, and exposure controls in a separate ADR.

## Alternatives considered

### HTTP or HTTPS projection as the remote execution surface

This alternative is rejected because it would add a second request projection and a second internal protocol boundary. The cited source revision has no HTTP route table or product HTTP ingress, so this alternative would be new surface rather than an extension of the daemon's existing frames. Source: `crates/khive-mcp/src/transport.rs:34-90`; `crates/khive-runtime/src/daemon.rs:95-106,1492-1503`; and `crates/kkernel/src/cli.rs:259-283` at commit `e40e0392f`.

### MCP as the internal fabric

This alternative is rejected because the existing MCP surface is stdio transport and does not provide a remote authenticated frame transport in the cited source revision. MCP remains appropriate as a compatibility adapter at the boundary. Source: `crates/khive-mcp/src/transport.rs:34-90` at commit `e40e0392f`.

### Client-asserted remote actor identity

This alternative is rejected because the current daemon accepts the relevant identity fields from the connecting process. A remote caller could otherwise assert a different actor or namespace. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at commit `e40e0392f`.

### A policy check in one dispatch path

This alternative is rejected because chain execution and single or parallel execution follow distinct paths before registry dispatch. A one-path check would leave a route outside the peer-class allowlist. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at commit `e40e0392f`.

### Loopback TCP binding

This alternative is rejected because every local process already has an authenticated local path — the Unix-domain socket with operating-system peer credentials — while a loopback TCP peer offers no equivalent non-forgeable fact for the whois-based mapping to consume. Admitting loopback would create a second local ingress with weaker identity than the one it duplicates.

### Local fallback or daemon auto-spawn for remote endpoints

This alternative is rejected because it would make a remote endpoint failure capable of selecting a local authority path. ADR-049 permits defined local fallback behavior for its local Unix-socket client, but that behavior is not suitable for an explicit remote endpoint. Source: ADR-049, Decision section 2 and Amendment 2.

### Non-tailnet binding

This alternative is rejected because this ADR does not define public-network authentication or transport-security controls. The absence of a current TCP product listener is measured, but the security requirements for a future public listener are UNVERIFIED and require a separate ADR. Source: `crates/khive-runtime/src/daemon.rs:95-106,1492-1503` at commit `e40e0392f`.
