# khive-capability

Process-local sealed grants, attenuating delegation and namespace-scoped validation
over one `khive-lion-core` kernel.

This crate is ported from `khive-capability` version 0.6.0 at commit
`341cd4dced8727fa983b20fd3266138d99cf109d`. It now builds on `khive-lion-core`,
the workspace copy of `lion-core` 0.4.0, and is licensed Apache-2.0.

## Library surface

- `ActorRegistry` retains session-to-actor bookkeeping.
- `NamespaceMap` retains the deterministic BLAKE3 namespace-to-resource mapping.
- `RequestContext` takes a validated `khive_types::Namespace` and carries session/task scope.
- `CapabilityManager::with_key` takes a caller-supplied random 32-byte process-local key
  and refuses an all-zero key. It does not generate the seal key or fall back to a default key.
- `mint` returns a sealed grant with the requested nonempty rights for a registered actor.
  `bootstrap_root_capability` is a compatibility alias for this sealed mint.
- `compose` checks the held parent's seal and full ancestor liveness, then lets the
  kernel intersect requested rights with the parent's rights. An empty intersection fails.
- `get_cap` returns canonical public grant records, including revoked ones;
  `is_live` checks the stored grant and every ancestor through a known root anchor.
- `validate` combines held canonical identity, seal, complete ancestor liveness,
  namespace and required-right checks. `revoke` invalidates a grant and its descendants.
- `RequestValidator` retains the exhaustive finer operation-to-right classification
  and its separate Read/Write binding projection.

The manager is a trusted authority-management object. Its mint, compose and revoke
methods do not identify or authorize an external caller. A future caller-facing
adapter must enforce that policy before invoking them. This crate does not install
a runtime `Gate`, create a persistent grant store, mint grants at boot or support
cross-process seals. `khive-gate` remains the separate authorization-decision interface;
its coarse `OperationAccess` is not substituted for this crate's finer rights table.

## Seals and roots

The core exposes sealed delegation but no direct parentless-root sealing API. Each
mint therefore creates one private raw anchor with that mint's namespace and rights,
then returns the kernel-sealed child. Anchors are never returned by `get_cap` or
accepted as public grants by validation, composition or revocation. A grant's signed
parent reference may name its anchor; that reference does not expose anchor authority.
Public grant IDs occupy the low half of the ID space. Anchors descend from
`u128::MAX - 1` in the high half; the sentinel `u128::MAX` is excluded and exhaustion
is refused before the ranges can overlap. Independent mints have independent anchors.

`verify_seal` is a pure cryptographic predicate. The core seal authenticates holder,
namespace resource, rights, parent and epoch; it does **not** authenticate the ID or
mutable revocation flag. Successful verification alone does not prove table membership,
holder authority or liveness. Authority checks use the manager's canonical stored
records; caller-supplied records are not inserted through this API.

The ancestor walk is iterative and bounded by the number of stored capabilities,
refusing missing nodes, cycles and unknown roots. It does not assume parent IDs are
smaller than child IDs. A liveness check is a point-in-time observation: a consumer
must keep its own synchronization through a later protected write if revocation can
race that write. No new transaction fence or persistence guarantee is implied.
