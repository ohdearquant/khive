# ADR-050: KG Token Namespace Contract

> **Partially superseded by ADR-007 Rev 3 (2026-06-17, which replaces Rev 2 in full).** The
> §"Decision" clause (removal of the KG-pack namespace override) is absorbed and confirmed by
> ADR-007 Rev 3 Rule 2 and Rule 3. The by-ID namespace check aspects are superseded by ADR-007
> Rev 3 Rule 2 (SHIPPED, PR-A1).
> The §"Context" and §"Rationale / Why not token rebinding" sections remain authoritative
> historical background for why the override was introduced and why it was removed.

**Status**: Accepted — core decision shipped with initial codebase; §"Decision" absorbed and
confirmed by ADR-007 Rev 3 (#164, 2026-06-17). See supersession note above.
**Date**: 2026-06-03
**Supersedes**: ADR-007 §"Namespace-by-Layer Rule" (KG pack rebinding only)
**References**: ADR-007 (Rev 3), ADR-028, ADR-029

---

## Context

`VerbRegistry::dispatch` is the namespace boundary for all verb operations. It resolves the
operation namespace from `params["namespace"]` when present, falls back to
`self.default_namespace`, validates the namespace, asks the gate, mints a `NamespaceToken`,
strips the raw `namespace` field from params, then calls `Pack::dispatch(..., token)`.

The KG pack (`khive-pack-kg`) receives the already-authenticated `NamespaceToken` but has
historically overridden it:

```rust
let graph_ns = self.runtime.config().default_namespace.clone();
let kg_token;
let graph_token = if token.namespace() != &graph_ns {
    kg_token = token.with_namespace(graph_ns);
    &kg_token
} else {
    token
};
```

This override existed to implement the ADR-007 "Namespace-by-Layer Rule" — KG entities and
edges were always forced into the shared `default_namespace` (`local`) so that cross-project
graph structure would be visible to all actors.

The override is problematic in deployments that derive namespace from an authenticated
`NamespaceToken`. `engine_config.rs:150-154` documents that such deployments ignore the
`[actor]` / `default_namespace` runtime config field. The KG pack override therefore pulls
entity/edge writes into a stale runtime-global namespace rather than the authenticated
namespace, causing data bleed across isolated actor namespaces.

## Problem

The KG pack namespace override violates the `NamespaceToken` contract established at the
registry boundary:

1. **Cross-namespace bleed**: An entity create arrives with a token carrying
   `namespace = actor-A`. The pack override reads `RuntimeConfig::default_namespace`,
   which is the runtime-global default rather than the authenticated namespace, and rewrites
   the token. Actor-A entities land in the wrong namespace.

2. **Explicit namespace suppression**: ADR-007's own namespace-resolution text
   (`ADR-007:389`) states that verbs supplying `namespace=` explicitly must use that value
   unconditionally. The KG pack override silently ignores explicit caller namespaces, creating
   an internal contradiction within ADR-007.

3. **Pack policy coupling**: The pack reads `RuntimeConfig::default_namespace` directly, making
   it a hidden policy dependency on the runtime config rather than a pure consumer of the
   dispatched token. This violates the typed boundary ADR-007 otherwise enforces.

## Decision

KG entity and edge verbs honor the `NamespaceToken` received from `VerbRegistry::dispatch`.
The pack must not read `RuntimeConfig::default_namespace` to override that token.

```rust
// KG graph operations honor the NamespaceToken minted by VerbRegistry::dispatch.
// Shared-namespace deployments rely on the registry/runtime default namespace;
// isolated-namespace deployments rely on the authenticated token namespace plus
// backend-file routing (ADR-050).
let graph_token = token;
```

Note scoping is **unchanged**: note, task, and event verbs already use the caller token
directly and are not affected by this decision.

The rejected `with_shared_graph_namespace` toggle must not be added. Adding it creates two
divergent namespace contracts and leaks routing policy into pack construction.

## Alternatives Considered

### Keep ADR-007 pack rewrite

The existing behavior continues to force all KG entity/edge writes into
`RuntimeConfig::default_namespace` regardless of the dispatched token. Rejected because:

- It violates authenticated tenant namespaces, causing cross-tenant bleed.
- It contradicts ADR-007's own explicit-namespace rule (`ADR-007:389`).
- It places policy inside the pack rather than at the authenticated dispatch boundary.

### Add `with_shared_graph_namespace(false)` toggle

A boolean field on `KgPack` would disable the override when set to false. Isolated-namespace
deployments would call `.with_shared_graph_namespace(false)`. Explicitly rejected by Ocean:

- Creates two contracts for the same pack; pack behavior becomes construction-time-dependent.
- Leaks routing policy into pack instantiation.
- Complicates deployments that might inadvertently pass the wrong value.

### Move isolation to backend-file routing plus token plus privilege (chosen)

ADR-028 provides per-tenant/pack backend file assignment. ADR-029 provides a
`SubstrateCoordinator` for operations that span multiple backends. The `NamespaceToken` is
already minted from authenticated context at the registry boundary. Together these three
mechanisms supply full tenant isolation without any pack namespace rewrite:

- Tenant namespace is authenticated and encoded in the token before pack dispatch.
- Tenant data lands on the correct backend file by ADR-028 routing.
- Cross-backend operations (when implemented) route through the ADR-029 coordinator.
- The KG pack is a pure consumer of the token it receives.

## Default-Deployment Preservation

The single-user unified graph is preserved without the pack override.

The default path sends no explicit `namespace=` argument. `VerbRegistry::dispatch` falls
back to `self.default_namespace`, which `KhiveRuntime` stamps from `actor.id` in the config
(`engine_config.rs:150`; `runtime.rs:837`). The MCP server plumbs this default into the
registry builder (`server.rs:178–181`). Every no-namespace-arg KG operation therefore lands
in the same actor default namespace — the unified graph survives.

The one intentional semantic change:

- **Before**: a caller passing an explicit different `namespace=` for a KG entity/edge
  verb was force-merged back into `default_namespace` by the pack override.
- **After**: that explicit namespace lands where the caller asked, subject to the same registry
  validation and gate semantics as every other pack.

This change is acceptable under ADR-007 by:

- `In_pari_materia`: ADR-007's own namespace-resolution amendment (`ADR-007:389`) already
  states explicit namespace values are used unconditionally. The pack override was an
  internal contradiction with that text.
- `Last_in_time`: Ocean's current intent (2026-06 decision) and ADR-028/029 make isolation a
  deployment routing and privilege concern, not a pack rewrite concern. The newer contract
  governs the conflict.
- `Constitutional`: removing the override reduces pack coupling from 7 to 6 approximate
  dependency edges, within the kappa < 0.3 target.

## Isolation Seam

```text
authenticated request
  → authenticated session / privilege gate
  → NamespaceToken(namespace = actor namespace)
  → backend-file route for this actor/pack  [ADR-028]
  → KgPack honors token                     [this ADR]
  → KhiveRuntime writes to routed backend under token.namespace()
```

Cross-backend operations (federated link, traverse, search) route through the
`SubstrateCoordinator` layer described in ADR-029. Pack crates do not own that routing.

Operators using custom binaries that previously called `with_shared_graph_namespace(false)`
must remove those call sites. The method no longer exists; the pack now honors the dispatched
token unconditionally. Isolated-namespace deployments should rely on per-actor backend routing
plus token namespace plus privilege gate.

## Migration and Compatibility

No schema migration is required.

**Default-namespace data** (no explicit `namespace=` arg): same runtime default namespace
as before; no data moves and no queries break.

**Explicit-namespace data**: entities that were previously force-merged into
`default_namespace` will now be created in the explicitly requested namespace. Existing data
already written under `default_namespace` via the override is not automatically migrated.
Callers that were relying on the implicit redirect must either stop passing an explicit
namespace (inheriting the default) or perform a targeted export/import under operator
control.

**Accidental cross-namespace data**: any tenant entities that landed in the stale
runtime-global namespace due to the override are not rewritten automatically. Remediation
requires operator-controlled export/import or targeted SQL backfill once tenant namespaces are
confirmed correct.

Compatibility guarantees:

- Public handler signatures are unchanged.
- Verb names and params are unchanged.
- `namespace` remains consumed at the registry boundary and is not forwarded as raw pack params.
- Existing default-namespace callers see no behavior change.
- Cross-namespace entity/edge reads continue to fail closed as `RuntimeError::NotFound`, avoiding
  an existence oracle.

## Implementation Notes

**Change**: Delete `graph_ns`, `kg_token`, and the conditional token rewrite from
`KgPack::dispatch`. Replace with `let graph_token = token;`. Keep the `verbs` early return.

**Tests updated**:

- `kg_create_entity_uses_local_namespace_regardless_of_caller_namespace` → replaced with
  `kg_create_entity_honors_caller_namespace`: creates under `tenant-a`, verifies `tenant-a`
  reads back the entity, asserts `tenant-b` gets `NotFound`.
- `namespace_token_with_namespace_preserves_actor` removed: this test documented the now-
  deleted pack dispatch simulation; `with_namespace` utility behavior belongs in runtime-token
  unit tests if needed.
- `kg_default_namespace_entities_colocate` added: two creates with the default `local`
  token, both readable from `local` — unified graph regression test.

**Stale comments updated** in `handlers.rs::handle_get`: removed "entities live in graph
namespace" / "entities and edges use graph namespace"; replaced with "graph token, which is
the caller token under ADR-050."

## Consequences and Risks

**Benefits**:

- Eliminates cross-tenant bleed in multi-tenant deployments.
- Aligns KG pack with the typed `NamespaceToken` contract established at the registry boundary.
- Reduces pack coupling (hidden `RuntimeConfig` dependency removed).
- Unblocks explicit-namespace callers to use the namespace they specify.

**Risks**:

- A caller who previously relied on the implicit redirect (passing `namespace=foo` but
  expecting entities in `local`) will now land in `foo`. This is the correct behavior per
  ADR-007:389 but is a semantic change for such callers.
- Operators using custom binaries that call `with_shared_graph_namespace(false)` must remove
  those call sites before deploying this change; the method no longer exists and the binary will
  fail to compile without removing them.

## Canons of Construction Applied

- `In_pari_materia`: ADR-007 sections read together; explicit-namespace rule (`ADR-007:389`)
  and the Namespace-by-Layer Rule (`ADR-007:445–498`) were internally inconsistent; the
  explicit-namespace rule is the more principled text.
- `Last_in_time`: Ocean's 2026-06 direction supersedes the 2026-05-27 Namespace-by-Layer Rule
  for KG pack rebinding.
- `Constitutional`: narrowing interpretation — removal reduces pack coupling below kappa 0.3
  threshold rather than widening it.
