# ADR-007 Rev 7: Namespace as Attribution-Only Open String — Dumb Storage, Single Gate, Operator-Configured Read Visibility

**Status**: Accepted
**Date**: 2026-06-19
**Authors**: khive maintainers

## Context

Earlier namespace designs mixed three concerns: record attribution, storage routing, and
authorization. That coupling introduced conflicting behavior between by-ID operations,
multi-record queries, and pack dispatch.

The accepted design separates those concerns:

- namespace is record attribution and a query filter;
- storage executes the scope supplied by its caller;
- authorization is enforced once, at the runtime Gate;
- operator configuration may widen the default read view without changing write routing.

This ADR specifies the namespace contract. It does not specify a multi-tenant storage
topology or an operator's authorization policy.

## Decision

### Rule 0 — One shared default namespace

The default namespace is `local`. Actor identity is attribution: it is stamped on write
records and included in Gate request context, but it does not silently become the storage
namespace.

An explicit `namespace=` request parameter may target a named namespace. Without that
parameter, writes use `local`.

### Rule 1 — Storage is dumb

Stores are unscoped database connections. Namespace is a column on each record, written
as supplied by the record.

- Multi-record methods such as `list`, `search`, `neighbors`, `traverse`, and
  `query` accept a caller-supplied namespace scope used in their query predicates.
- By-ID methods such as `get`, `update`, and `delete` use the globally unique record
  identifier and do not add a namespace-equality predicate.

Handlers and stores must not introduce inline authorization checks. The Gate is the
authorization seam.

### Rule 2 — By-ID operations are namespace-agnostic

`get`, `update`, and `delete` by UUID resolve a globally unique identifier with no
namespace check in storage or as a runtime post-fetch step.

This is a lookup contract, not an authorization bypass. The runtime checks the Gate before
dispatch.

### Rule 3 — Multi-record operations use an explicit read scope

Without an explicit namespace request, multi-record reads use the default visible set:

```text
["local"] ∪ visible_namespaces
```

`visible_namespaces` is operator-configured, normalized, deduplicated, and validated with
`Namespace::parse`. The set is a read-view configuration, not a storage partition and not
an authorization mechanism.

An explicit `namespace=X` request targets exactly `X`. It is not widened by the default
visible set. This preserves precise access to a named set:

```text
list()                    → ["local"] ∪ visible_namespaces
list(namespace="ns-a")   → ["ns-a"]
create(...)               → "local"
create(namespace="ns-a") → "ns-a"
```

The runtime supplies the resulting read set to storage as a
`WHERE namespace IN (...)` filter. Writes remain single-target operations.

### Rule 3a — Edge namespace is attribution-only

Every edge record carries its own `namespace` column. By default it is stamped `local`.
It is not derived from the source or target endpoint and is not used as an access-control
boundary.

Queries do not apply a three-column visibility join across the edge and both endpoints.
Endpoint existence and relation validity remain separate graph-integrity concerns.

### Rule 4 — Authorization is enforced at one seam

`VerbRegistry::dispatch` calls the configured `Gate` before every verb invocation.
This is the single enforcement point.

- `AllowAllGate` is the permissive default.
- An operator-provided Gate may validate authenticated identity and apply policy using
  request and attribution fields.

Namespace may be a Gate policy input, but it never becomes a storage-level authorization
check. Storage behavior and by-ID lookup semantics remain identical for every Gate
implementation.

### Rule 5 — Merge guards are substrate semantics

The same-namespace merge guard is retained as a curation constraint for same-substrate
deduplication. It is not an isolation mechanism and must not be treated as one.

### Rule 6 — Namespace is an open string with a validated factory

Namespace is a string-backed newtype constructed through `Namespace::parse`.
Validation requires:

- non-empty input;
- at most 256 characters;
- characters from `[a-zA-Z0-9\-_:.]`;
- no trailing separator;
- no empty segments.

`Namespace::local()` returns the `local` singleton. The namespace vocabulary remains
open; validation prevents malformed attribution values without turning them into a closed
ontology.

### Rule 7 — Writes stamp attribution

Writes stamp namespace, actor identifier, and actor kind from the dispatch context.
Attribution is queryable, filterable, loggable, and available to the Gate as policy input.
It does not select a backend or grant access.
