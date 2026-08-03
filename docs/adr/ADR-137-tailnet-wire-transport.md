# ADR-137: Tailnet Wire Transport for the khive Frame Protocol

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

The present daemon uses a Unix-domain socket at `~/.khive/khived.sock`, accepts length-prefixed JSON frames, and checks the local peer UID reported by the operating system. The observation was measured on a development deployment at `origin/main` commit `e40e0392fba5ec7f9e4c7d406b68e35799f8087c`. Source: `crates/khive-runtime/src/daemon.rs:95-106,306-320,607-678,1443-1503,1608-1651` at `origin/main` `e40e0392f`.

No authenticated TCP or HTTP ingress for the khive verb surface exists in that measured source revision. The built-in foreground transport is MCP over stdio, and the transport registry registers `StdioTransport` only. Source: `crates/khive-mcp/src/transport.rs:34-90`; `crates/kkernel/src/cli.rs:259-283`; and `crates/khive-runtime/src/daemon.rs:95-106,1492-1503` at `origin/main` `e40e0392f`.

The existing daemon protocol accepts frame-carried `namespace`, `actor_id`, and `visible_namespaces` from the connecting process, while the Unix peer UID is the non-forgeable local fact. A remote transport must therefore establish identity at server ingress instead of trusting those frame fields. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at `origin/main` `e40e0392f`.

The current request surface has two execution paths after parsing: chain mode reaches `dispatch_op`, while single and parallel modes reach `dispatch_with_identity` separately. A policy check inserted in only one path would not constrain the other path. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at `origin/main` `e40e0392f`.

ADR-049 establishes the warm daemon, its Unix-socket framing, and its local client recovery behavior. Source: ADR-049, Decision sections 1 through 4 and Amendment 2. ADR-109 establishes the constrained-caller policy requirements, including a closed allowlist and fail-closed gateway behavior. Source: ADR-109, Decision and Hard rules.

## Decision

khive will add a second transport for the existing frame protocol. The transport is a TCP listener that may bind only to loopback addresses and to explicitly configured tailnet interface addresses. It must not bind wildcard, public, or other non-tailnet addresses.

### Shared protocol crate and handshake

A shared Rust crate, `khive-wire-protocol`, will define framing, the handshake, frame kinds, protocol versioning, and protocol errors. Its crate documentation is the normative wire specification. Rust clients will link this crate, while non-Rust clients will implement that specification directly.

The existing length-prefixed JSON framing remains the base framing. The first application frame on every TCP connection must be a handshake carrying a protocol version. The server must reject an unsupported version before it accepts a request or subscription frame, and the client must surface that rejection without fallback.

The protocol will define request, response, error, handshake, subscription, and state-change frame kinds. A subscription frame declares an allowed subscription class. The server pushes state-change frames for that class on the same connection. The server must reject a subscription that the peer class is not allowed to receive.

### Transport identity and actor mapping

Unix-domain-socket handling remains local. It continues to derive local peer facts from the operating system credential mechanism represented by the current `SO_PEERCRED` or equivalent path. Source: `crates/khive-runtime/src/daemon.rs:306-320,1608-1651` at `origin/main` `e40e0392f`.

For a TCP connection, the server must invoke `tailscale whois` against the connecting address and derive the peer identity from that server-side result. The server must then resolve that peer identity through an explicit server-side mapping table that assigns both a khive actor and a peer class. Failure of the lookup, absence of a mapping, or an invalid mapping must reject the connection. An unmapped peer must never receive a default actor.

On the TCP transport, frame-carried `namespace`, `actor_id`, and `visible_namespaces` are ignored for identity and authorization. The mapped actor, mapped peer class, and server policy determine the request context. This rule closes the distinction between caller-supplied frame fields and operating-system-proven local credentials observed in the current daemon. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at `origin/main` `e40e0392f`.

The TCP transport does not add TLS inside the tailnet. This decision relies on WireGuard transport encryption supplied by the tailnet as a deployment precondition. The encryption guarantee is external to khive and is UNVERIFIED by the cited `origin/main` source. Binding any non-tailnet address is out of scope for this ADR and requires a separate ADR with its own transport security decision.

### Peer classes and dispatch enforcement

The server must enforce a closed verb allowlist for every peer class at the dispatch chokepoint before execution. A read-only peer class is defined as a mapped class whose allowlist contains only explicit read verbs and permitted subscription classes. It must not include a state-mutating verb or state-mutating protocol frame.

Both dispatch paths are a correctness condition, not an implementation preference. The enforcement must cover chain mode through `dispatch_op` and single or parallel mode through `dispatch_with_identity`; a check that covers only one path is non-conformant. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at `origin/main` `e40e0392f`.

The all-mode integration point must reject every parsed operation outside the mapped peer class before `run_parsed` dispatches it. This placement follows from the distinct current execution paths and is not an existing implementation. Source: `crates/khive-mcp/src/server.rs:2038-2122` and `crates/khive-mcp/src/server.rs:1031-1165` at `origin/main` `e40e0392f`.

### Remote endpoint behavior

Remote endpoint mode is explicit configuration, not an inferred recovery state. A client configured for a remote endpoint must not create a local runtime, fall back to local dispatch, or auto-spawn a daemon when the remote endpoint is unavailable, rejects a handshake, or cannot be reached.

This differs deliberately from ADR-049's local client behavior, which includes daemon recovery and defined local fallback classes. Source: ADR-049, Decision section 2 and Amendment 2. Remote endpoint failure is a surfaced failure because substituting a local runtime could create a divergent authority or data path.

### Interop edge: inner ring and outer ring

The transport surface has two rings. The **inner ring** is the native frame protocol defined above: the Unix-domain-socket and tailnet-TCP connections carrying the shared `khive-wire-protocol` framing, reserved for first-party khive processes and for constrained callers admitted under this ADR's identity and peer-class rules. Nothing outside this ADR's native framing is ever admitted to the inner ring, and internal khive processes use native frames or in-process dispatch exclusively.

The **outer ring** is a first-class, gated boundary surface, not a compatibility shim, and it is bidirectional:

- **Inbound.** A remotely hosted agent or MCP-speaking client reaches khive only through a standard-protocol edge adapter, which may include a Streamable-HTTP-class MCP transport as a supported product surface where a deployment enables one. The edge adapter never gains direct access to the inner-ring frame protocol or an unconstrained dispatch surface: it terminates the standard protocol, resolves the caller's identity using this ADR's server-side peer-mapping rule exactly as the native TCP transport does, and then issues native-frame or in-process dispatch calls to the shared dispatch chokepoint on the caller's behalf. The per-peer-class allowlist enforcement defined in "Peer classes and dispatch enforcement" above applies to every operation the edge adapter issues; the adapter is a mapped peer class like any other caller, never an implicitly trusted internal client.
- **Outbound.** khive may mount an external standard-protocol tool server (for example, a hosted git service's remote MCP endpoint) as a tool source available to owned agents. An outbound mount is registered through the same single dispatcher used for in-process verbs; a mounted tool call receives the same capability scoping and audit treatment as an in-process verb, and no execution path reaches a mounted outbound tool server without passing through that dispatcher.

The inner/outer distinction is a hard boundary, stated once and applied everywhere in this ADR: a standard protocol such as MCP is never the internal fabric between first-party khive processes, in either direction, and the inner ring's native framing is never exposed directly to a standard-protocol client. Enabling an inbound edge adapter for a given deployment is a configuration decision that must confirm the identity-mapping and peer-class-allowlist requirements above are satisfied before the adapter accepts a connection; it does not require a new ADR, but it does require the same server-side identity and allowlist mechanism this ADR defines for the native TCP transport.

## Protocol contract completeness

This ADR treats the framed protocol as a product contract, not a private daemon detail, and specifies the following wire-level mechanics normatively:

- **Canonical schema.** The `khive-wire-protocol` crate's documentation is the normative wire specification, and its schema is versioned together with the handshake's protocol version; a schema change that is not backward-compatible requires a version bump.
- **Capability negotiation.** The handshake frame carries the client's supported protocol version; a server rejects an unsupported version before accepting any other frame, as already specified above.
- **Request correlation.** Every request frame carries a caller-generated request id, unique for the lifetime of the connection; the server echoes that id on the matching response, error, or cancellation-acknowledged frame, so a caller can correlate concurrent in-flight requests on one connection.
- **Stable error taxonomy.** Wire-level errors are a closed set distinct from verb-level DSL errors (ADR-016): at minimum `unsupported_version`, `identity_rejected`, `peer_class_denied`, `frame_too_large`, `deadline_exceeded`, `subscription_denied`, and `malformed_frame`. A wire-level error is returned before any DSL dispatch is attempted; a DSL-level per-op error (ADR-016's `ok: false` result) is a distinct, later failure mode.
- **Limits.** Every deployment configures a maximum frame size and a maximum number of in-flight requests per connection; a frame exceeding the configured size is rejected with `frame_too_large` and the connection is closed rather than partially buffered.
- **Deadlines and cancellation.** A request frame may carry an optional deadline; a server that cannot complete the request before that deadline abandons the work and returns `deadline_exceeded`. A dedicated cancellation frame kind, referencing a request id, allows a client to cancel a request that carried no deadline.
- **Event cursor and reconnect.** A subscription frame's state-change deliveries carry a resumption cursor; a reconnecting client may request replay from its last-acknowledged cursor within a server-defined retention window, and a cursor outside that window is rejected with a distinct error rather than silently resuming from an arbitrary point.
- **State-change topic namespace and payload envelope.** This ADR, not a downstream crate or client contract, owns the subscription topic namespace and the state-change payload envelope. A topic is a closed, versioned string of the form `<domain>.<event>` (for example `comm.message_created`); every state-change frame carries the envelope `{ topic, cursor, occurred_at, payload }`, where `cursor` is the resumption cursor defined immediately above, `occurred_at` is the server-assigned event time, and `payload` is a topic-specific JSON object whose exact field-by-field shape this ADR delegates by name, not by silence, to the deferral below. The subscription authorization hook is the peer-class allowlist already defined in "Peer classes and dispatch enforcement": a subscription frame names exactly one topic, and the server rejects that subscription, before any frame for it flows, if the caller's mapped peer class does not include that topic in its allowed-subscription set. Frames for one topic on one connection are delivered in non-decreasing cursor order with no gaps inside the retention window specified above. Only the exhaustive topic catalog and each topic's `payload` field schema are implementation-phase deliverables; the envelope, the ordering guarantee, and the authorization hook are normative in this ADR.
- **Backpressure.** Each connection's outbound state-change queue is bounded; reaching that bound disconnects the subscriber with a defined error rather than growing server memory without limit.
- **Compatibility policy.** Protocol version numbers are monotonic; a server supports the current version and at least the immediately prior version, and a breaking wire change is never introduced within a version number.

The exhaustive wire-level error code table, the exact numeric defaults for frame-size and in-flight-request limits, a golden-frame fixture suite, decoder fuzzing, cross-client conformance testing, and the exhaustive per-topic subscription payload catalog (the concrete list of topics and each topic's `payload` field schema, within the envelope this ADR defines above) are implementation-phase deliverables owned by the `khive-wire-protocol` crate maintainer. Enabling TCP subscriptions, or any inbound edge adapter, for a peer class beyond loopback is gated on that conformance suite passing, including conformance coverage of the published topic catalog; this ADR does not authorize enabling subscriptions for a non-loopback peer class before the gate is satisfied.

### MCP placement

MCP over stdio remains a boundary compatibility adapter for foreign agent runtimes reached through the outer ring defined above. It is not the internal fabric. An MCP adapter that reaches a remote endpoint uses this ADR's native frame protocol rather than defining a competing internal protocol.

## Supersession and amendment of ADR-109

This ADR amends one specific part of ADR-109: the section "Fork (a): Process boundary" and its "Resolution (Open Question 1 - process boundary)" paragraph, which selects a thin gateway binary that "connects to the warm daemon as a client, a proxy" without specifying the transport that connection uses. This ADR supplies that transport: a tailnet-connected gateway binary connects to the daemon over this ADR's native frame protocol, with the daemon performing server-side peer-identity mapping and peer-class allowlist enforcement at the shared dispatch chokepoint, rather than through an MCP stdio or HTTP-facing remote executor. Source: ADR-109, "Fork (a): Process boundary" and its Resolution.

ADR-109's "Hard rules (not forked)" 1 (closed, explicit verb allowlist), 2 (pinned, non-caller-suppliable namespace), and 6 (fail-closed on anything outside the contract) remain in force unchanged for a peer class defined by this ADR; this ADR's server-side peer-class allowlist is an additional transport-layer enforcement point, not a replacement for those rules. Source: ADR-109, "Hard rules (not forked)", rules 1, 2, and 6.

This amendment is limited to the gateway's process-boundary transport, ingress identity, and the corresponding dispatch integration. ADR-109 remains the authority for its capability declaration format, authentication resolution, and Phase B relationship (Forks (b), (c), and (d)), which this ADR does not amend. Source: ADR-109, "Decision" and "Resolutions". A future revision of ADR-109 should add a forward reference to this ADR under its "Fork (a): Process boundary" section, naming this ADR as the source of the process-boundary transport it left open.

## Consequences

The daemon gains a location-transparent wire transport while retaining one framing and dispatch contract. The shared crate makes version and frame-kind changes reviewable as protocol changes instead of client-specific conventions.

The server becomes responsible for remote identity proof, actor mapping, and peer-class admission. This increases operational responsibility for the mapping table and for availability of `tailscale whois`, but it prevents a remote caller from selecting an actor through frame content.

Subscription delivery becomes part of the same authenticated connection as requests, using the topic namespace, payload envelope, ordering guarantee, and authorization hook this ADR defines in "Protocol contract completeness." Only the exhaustive per-topic payload catalog remains an implementation-phase deliverable, gated on the conformance suite as specified there.

Remote clients receive explicit endpoint failures rather than implicit local recovery. Deployments must therefore provision an endpoint selector, connection diagnostics, and operator remediation for unavailable remote services.

No general public-network listener is introduced. A deployment that needs a non-tailnet listener must first define authentication, encryption, and exposure controls in a separate ADR.

## Alternatives considered

### HTTP or HTTPS projection as the remote execution surface

This alternative is rejected because it would add a second request projection and a second internal protocol boundary. The measured source revision has no HTTP route table or product HTTP ingress, so this alternative would be new surface rather than an extension of the daemon's existing frames. Source: `crates/khive-mcp/src/transport.rs:34-90`; `crates/khive-runtime/src/daemon.rs:95-106,1492-1503`; and `crates/kkernel/src/cli.rs:259-283` at `origin/main` `e40e0392f`.

### MCP as the internal fabric

This alternative is rejected because the existing MCP surface is stdio transport and does not provide a remote authenticated frame transport in the measured source revision. MCP remains appropriate as a compatibility adapter at the boundary. Source: `crates/khive-mcp/src/transport.rs:34-90` at `origin/main` `e40e0392f`.

### Client-asserted remote actor identity

This alternative is rejected because the current daemon accepts the relevant identity fields from the connecting process. A remote caller could otherwise assert a different actor or namespace. Source: `crates/khive-runtime/src/daemon.rs:306-320,1055-1081` at `origin/main` `e40e0392f`.

### A policy check in one dispatch path

This alternative is rejected because chain execution and single or parallel execution follow distinct paths before registry dispatch. A one-path check would leave a route outside the peer-class allowlist. Source: `crates/khive-mcp/src/server.rs:1031-1165,2038-2122` at `origin/main` `e40e0392f`.

### Local fallback or daemon auto-spawn for remote endpoints

This alternative is rejected because it would make a remote endpoint failure capable of selecting a local authority path. ADR-049 permits defined local fallback behavior for its local Unix-socket client, but that behavior is not suitable for an explicit remote endpoint. Source: ADR-049, Decision section 2 and Amendment 2.

### Non-tailnet binding

This alternative is rejected because this ADR does not define public-network authentication or transport-security controls. The absence of a current TCP product listener is measured, but the security requirements for a future public listener are UNVERIFIED and require a separate ADR. Source: `crates/khive-runtime/src/daemon.rs:95-106,1492-1503` at `origin/main` `e40e0392f`.

## Evidence and ADR allocation record

The source observations in this ADR were measured on a development deployment at `origin/main` `e40e0392f`. Source: the `origin/main` file and line references in the Context and Decision sections.

The allocation sweep on 2026-08-03 checked the public ADR index and open pull requests against this public repository for numbering collisions: the highest listed numeric ADR was ADR-136, and no open pull request title or branch named an ADR number. This draft takes the next available number, ADR-137.
