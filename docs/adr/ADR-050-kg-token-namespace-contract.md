# ADR-050: KG Token Namespace Contract

> **Partially superseded by ADR-007 Rev 3.** ADR-007 owns namespace resolution and
> visibility. This ADR remains authoritative for the KG pack's token-consumption rule.

**Status**: Accepted\
**Date**: 2026-06-03\
**Supersedes**: ADR-007's earlier KG pack rebinding rule\
**Depends on**: [ADR-007](./ADR-007-namespace.md),
[ADR-028](./ADR-028-pack-scoped-backends.md),
[ADR-029](./ADR-029-substrate-coordinator.md)

---

## Context

`VerbRegistry::dispatch` resolves the effective namespace, validates it, evaluates the
authorization gate, and mints a `NamespaceToken`. It removes the raw namespace parameter before
calling a pack. The token is therefore the typed namespace capability presented to pack handlers.

If a pack independently consults runtime configuration and rewrites that token, dispatch and
storage can disagree about the authorized namespace. The result would violate the capability
boundary and make pack behavior depend on hidden configuration.

## Decision

KG entity, edge, note, event, and proposal operations use the `NamespaceToken` received from
dispatch. The KG pack must not replace its namespace from `RuntimeConfig::default_namespace` or
from a verb parameter.

```rust
// Dispatch has already resolved and authorized the namespace.
let graph_token = token;
```

The same rule applies to by-ID reads and writes. Resolving an identifier does not grant access to a
record outside the token's visible namespaces.

### Token responsibilities

The contract is divided as follows:

| Layer            | Responsibility                                                        |
| ---------------- | --------------------------------------------------------------------- |
| Request parser   | Parse the optional namespace argument without interpreting policy     |
| Verb registry    | Resolve, validate, authorize, and remove the raw namespace argument   |
| `NamespaceToken` | Carry the authorized write namespace and visible namespace set        |
| KG pack          | Pass the token to runtime operations without widening or rebinding it |
| Runtime stores   | Scope every query and mutation to the token                           |

Backend selection remains a separate concern under ADR-028. Selecting a backend does not change
the token, and the token does not select a backend by itself.

## Invariants

1. A KG handler never reads a raw namespace parameter.
2. A KG handler never derives a replacement namespace from runtime configuration.
3. Writes use the token's write namespace.
4. Reads use only the token's visible namespace set.
5. By-ID lookup outside the visible set returns `NotFound` and does not reveal existence.
6. Nested KG operations receive the same token or a strictly narrower token.
7. Backend routing cannot widen namespace visibility.

## Default local behavior

When a request does not specify a namespace, the registry uses the runtime's configured default.
The KG pack then receives and honors the resulting token. Repeated local operations therefore
continue to use one consistent graph without a pack-specific override.

When a request surface accepts an explicit namespace, ADR-007 determines whether that namespace is
permitted and how it affects visibility. This ADR adds no alternate resolution path.

## Rejected alternatives

### Rebind every KG token to the runtime default

Rejected because the pack would discard the result of dispatch-time resolution and gate
evaluation. It would also make explicit namespace behavior differ between the KG pack and other
packs.

### Add a `with_shared_graph_namespace` toggle

Rejected because construction-time configuration would create two namespace contracts for the
same verb surface. Shared local behavior is already represented by the registry's default
namespace.

### Accept namespace fields in individual handlers

Rejected because each handler could interpret or validate the field differently. Namespace
resolution belongs at the single dispatch seam.

## Migration and compatibility

No schema migration is required. Handler signatures and verb parameters are unchanged.

Applications that previously depended on pack-level token rebinding must stop doing so and use the
registry's namespace configuration. Existing records are not moved automatically; ordinary public
export and import operations can be used when an application intentionally reorganizes its local
namespaces.

## Implementation notes

- `KgPack::dispatch` passes the received token directly to every handler.
- The special `verbs` catalog path remains side-effect free and does not access substrate stores.
- Handler helpers accept `&NamespaceToken` instead of an independent namespace string.
- Tests use neutral names such as `namespace-a` and `namespace-b`.

Required regression cases:

- a record created with a token for `namespace-a` is readable with that token;
- the same identifier returns `NotFound` under a token limited to `namespace-b`;
- default local operations remain colocated;
- an explicit request namespace is removed before pack dispatch;
- backend routing leaves the namespace token unchanged.

## Consequences

### Positive

- The typed dispatch boundary is authoritative.
- Pack behavior no longer depends on hidden runtime namespace configuration.
- Explicit and default namespaces follow one resolution contract.
- By-ID operations preserve the same visibility rules as list and search.

### Tradeoffs

- Applications cannot use a pack constructor to silently merge namespaces.
- Local namespace reorganization is an explicit data-management operation.
- All KG handler implementations must consistently thread the token.

## Technical consistency rationale

ADR-007 defines namespace resolution, ADR-028 defines backend assignment, and ADR-029 defines
coordination across backends. This ADR assigns the remaining pack-level responsibility: consume the
authorized token unchanged. Keeping these responsibilities separate avoids contradictory routing
rules and keeps the KG pack independent of deployment-specific configuration.
