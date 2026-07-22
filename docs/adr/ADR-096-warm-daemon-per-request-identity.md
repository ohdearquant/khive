# ADR-096: Per-request identity in the warm daemon

**Status**: Accepted
**Date**: 2026-07-05
**Authors**: khive maintainers
**Amends**: [ADR-049](./ADR-049-khived-daemon.md) (daemon request-frame identity)
**Relates to**: [ADR-007](./ADR-007-namespace.md) (namespace and attribution),
[ADR-018](./ADR-018-authorization-gate.md) (authorization), and
[ADR-091](./ADR-091-wal-snapshot-lifetime.md) (transaction lifetime)

## Context

The warm daemon keeps the embedding and retrieval state resident across client connections.
Engine configuration must remain stable for every request served by one daemon, but attribution
belongs to the individual request. Treating both concerns as construction-time daemon state would
either misattribute writes or force a compatible client onto the cold local path.

The configuration sources also have different anchors:

- engine-defining settings such as the database, pack set, embedding models, and backend routing
  are resolved relative to the database configuration so clients sharing an engine compute the
  same `config_id`; and
- the attribution actor is resolved for each connecting process from its project or working
  directory, with explicit command-line and environment overrides.

The accepted design separates these concerns and carries the resolved request identity through the
warm protocol without changing the engine identity.

## Decision

### 1. Resolve attribution independently of engine configuration

The serve path resolves the attribution actor using this precedence order, highest first:

1. an explicit `--actor` command-line value;
2. `[actor].id` in the project or working-directory `.khive/config.toml`;
3. `KHIVE_ACTOR`; and
4. anonymous attribution when none is set.

Project actor discovery reads only `[actor].id`. It does not alter the database target, pack set,
embedding configuration, backend routing, `config_id`, or `default_namespace`. An explicit
configuration path applies to both engine and actor discovery, but each resolver still consumes
only the fields it owns.

`KHIVE_ACTOR` is an attribution-only fallback. It does not implicitly set the storage namespace.
An explicit namespace remains a separate input under ADR-007.

### 2. Carry request identity on the daemon frame

Every normal daemon request carries a `RequestIdentity` with:

```rust
pub struct RequestIdentity {
    pub namespace: String,
    pub actor_id: Option<String>,
    pub visible_namespaces: Vec<String>,
    pub request_id: Option<u64>,
}
```

The wire protocol version covers this frame shape. A protocol-version mismatch uses the existing
safe fallback behavior. The daemon continues to reject an incompatible `config_id` because that
identifier protects engine coherence, not attribution.

### 3. Apply identity at dispatch time

The warm daemon uses one shared registry and passes `RequestIdentity` into dispatch for each
request. Dispatch derives the authorization context, storage namespace, write attribution, and
audit request identifier from that request identity. Construction-time identity values remain the
default only for an identity-less in-process dispatch.

The following invariants are binding:

- a warm-served write is attributed to the request actor, not the daemon process actor;
- reads and writes use the request namespace and visibility set;
- engine configuration is never selected or widened by request identity;
- `config_id` mismatch remains a hard rejection; and
- identity fields are excluded from `config_id`.

### 4. `config_id` remains an engine-coherence key

`config_id` covers the pack set, database target, primary and additional embedding models,
backend topology and routing, and any construction-time outbound policy. It excludes
`namespace`, `actor_id`, `visible_namespaces`, and `request_id`.

This separation allows two local clients with different attribution actors to share a compatible
warm engine while ensuring that clients with incompatible engine configuration cannot reuse it.

### 5. Local trust boundary

This decision applies to the local daemon transport protected by the operating system's account
and file-permission boundary. Request identity is an attribution and authorization input within
that boundary. It is not a replacement for transport authentication in a deployment that admits
connections from different security principals.

The local daemon MUST keep its socket owner-only, and the database MUST remain accessible only to
the intended local account. A different transport or principal model requires a separate design
with authenticated connection identity before it can reuse this request-identity contract.

## Verification

Regression coverage must prove:

1. A project containing `.khive/config.toml` with `[actor].id` resolves that actor even when the
   database configuration is anchored elsewhere.
2. Precedence is `--actor`, project configuration, `KHIVE_ACTOR`, then anonymous.
3. Actor discovery changes neither `default_namespace` nor `config_id`.
4. Two clients sharing one database but using different project actors compute identical
   `config_id` values.
5. A warm-served `create` is stamped with the requesting actor and remains observable through a
   subsequent warm-served `get`.
6. The daemon rejects an incompatible `config_id` and a protocol-version mismatch follows the
   defined fallback path.
7. Identity-less local dispatch continues to use the registry defaults.

## Alternatives considered

### Construction-time daemon identity only

Rejected because one long-lived process can serve requests from more than one local attribution
source. A process-wide actor cannot preserve request-level write provenance.

### One warm registry per attribution actor

Rejected because attribution does not define separate embedding or retrieval state. Replicating
registries would multiply resident state without improving correctness.

### Environment-only actor injection

Rejected as the primary mechanism because a project configuration is durable, discoverable, and
does not require launcher-specific environment setup. The environment remains an explicit
fallback.

### Include request identity in `config_id`

Rejected because it would turn attribution changes into engine incompatibilities and force cold
fallbacks even when all engine-defining settings match.

## Consequences

### Positive

- Warm serving preserves per-request write attribution.
- Project-specific actors work without changing shared engine configuration.
- The daemon keeps one resident registry for one compatible engine.
- Configuration mismatch continues to protect pack, database, embedding, and backend coherence.

### Negative

- The protocol frame and dispatch seam carry an additional identity value.
- Actor and engine configuration require separate resolution paths and precedence tests.
- The local account and socket permissions remain part of the transport trust boundary.

## Non-goals

- This ADR does not change the entity, edge, or note taxonomy.
- This ADR does not change checkpoint behavior or transaction lifetime.
- This ADR does not define a network listener or a new authentication protocol.
- This ADR does not make attribution identity part of engine configuration.

## References

- [ADR-007](./ADR-007-namespace.md): namespace and attribution semantics
- [ADR-018](./ADR-018-authorization-gate.md): authorization boundary
- [ADR-049](./ADR-049-khived-daemon.md): warm daemon and protocol fallback
- [ADR-091](./ADR-091-wal-snapshot-lifetime.md): transaction-lifetime constraints
