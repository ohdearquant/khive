# pack.rs — extended rationale

Long-form rationale extracted from `crates/khive-runtime/src/pack.rs` doc-comments during the
rustdoc condense pass. Each section links back to the item whose in-source doc-comment now
carries only the standalone contract plus a one-line pointer here.

## register_embedders

`PackRuntime::register_embedders` is called by the transport during pack initialisation, before
the first verb dispatch, so that `KhiveRuntime::embedder(name)` resolves provider names declared
here. Implement it to contribute non-lattice embedding backends:

```ignore
fn register_embedders(&self, runtime: &KhiveRuntime) {
    runtime.register_embedder(MyCustomProvider::new());
}
```

The default no-op preserves backwards compatibility — packs that only use built-in lattice models
do not need to override this method.

## register_entity_type_validator

`PackRuntime::register_entity_type_validator` is called by the transport during pack
initialisation, after the registry is built and before the first verb dispatch, so that
`create_many` and `create_entity` reject unregistered `entity_type` values at the runtime layer
in addition to the handler layer. Packs that own `EntityTypeRegistry` vocabularies (e.g.
`KgPack`) should override this to install their registry's `resolve` function. The default no-op
leaves the runtime validator absent (skip-when-None), which is the correct behaviour for bare
runtimes without packs.

This single-argument hook is intentionally left unchanged (not widened) so an out-of-tree pack
that already overrides it keeps compiling even if it declares no entity types. A pack that needs
the boot-time composed pack vocabulary should override `register_entity_type_validator_with_types`
instead — `call_register_entity_type_validators` calls that hook, not this one.

`register_entity_type_validator_with_types` receives the boot-time composed set of every loaded
pack's `ENTITY_TYPES` (`VerbRegistry::all_entity_types`) — the same aggregate every pack in the
loaded set receives, mirroring how `EDGE_RULES` are aggregated once and consulted by every pack.
It defaults to calling `register_entity_type_validator` with just the runtime, so a pack that
overrides only the older, simpler hook — or overrides neither — keeps compiling and behaving
exactly as before. `call_register_entity_type_validators` calls this hook, not the older one, so
a pack that wants the composed vocabulary must override this one. Packs that own
`EntityTypeRegistry` vocabularies should override this hook to compose
`EntityTypeRegistry::with_extra(pack_entity_types)` and install its `resolve` function.

## register_note_mutation_hook

Called by the transport during pack initialisation, after the registry is built and before the
first verb dispatch — same timing as `register_entity_type_validator`. Packs that cache derived
state keyed by note content (e.g. `khive-pack-memory`'s warm ANN index) should override this to
install a hook via `KhiveRuntime::install_note_mutation_hook`, so `update_note`/`delete_note`
notify them even when the mutation arrived through a different pack's verb that has no
dependency on the reacting pack (e.g. KG's `update`/`delete` on a `kind="memory"` note). The
default no-op leaves the runtime hook absent (skip-when-None), which is the correct behaviour for
packs that don't cache note-derived state and for bare runtimes without packs.

## registered_embedding_model_names

Used by ADR-103 Amendment 1's `model_count` computation at the dispatch audit-row emission seam
(`VerbRegistry::dispatch_with_identity`) for the two embedding-bearing verb families whose model
fan-out is not a per-dispatch constant: singleton `create` and `memory.remember` without an
explicit `embedding_model` override. Defaults to empty — only the packs that own those verbs (kg,
memory) need to override this by forwarding to their internal `KhiveRuntime`.

## dispatch_as

For embedding hosts (gateways, servers, or other processes that embed this runtime as a library)
that authenticate a principal through their own channel — not through the request DSL — and then
need that principal to be the effective actor for one dispatch. `verified_actor` is a typed
Rust-side argument: it can only be supplied by code holding a `VerbRegistry` handle. `dispatch_as`
never reads `params["actor"]` to derive the effective actor; individual verbs may still accept an
`actor` field for their own documented business semantics, unrelated to the acting principal.

`verified_actor` is a `VerifiedActor`, whose constructor rejects blank identifiers. This keeps an
authentication-integration failure (an empty subject from the host's own auth channel) from
silently downgrading to the anonymous/local actor — the failure surfaces at `VerifiedActor::new`
instead of being laundered into a valid dispatch.

Every pack handler that reads "who is calling" (for example, a proposal review's `reviewer`
field) resolves it from the `NamespaceToken` the dispatch boundary mints, so `verified_actor`
becomes exactly the principal those handlers observe.

## dispatch_with_identity

`identity = None` behaves exactly like `VerbRegistry::dispatch`. `identity = Some(id)` uses
`id.namespace` / `id.actor_id` / `id.visible_namespaces` in place of `self.default_namespace` /
`self.actor_id` / `self.visible_namespaces` for this call's namespace resolution, gate request,
and token minting — the registry's own fields are never mutated, so concurrent calls with
different (or no) identity are independent. This is what lets one warm registry correctly serve
requests from many attribution identities over the same shared backend (same db, same warm ANN
indexes) instead of rejecting or silently dispatching under its own baked identity (ADR-096 Fork
1).

## build_audit_storage_event

Shared by the immediate-append path (all verbs, denied calls, bulk `links`) and the deferred
singleton-`link` fallback so both audit shapes are produced by one code path.

`resource` is the ADR-103 `resource` payload object. ADR-103 Decision (a) stamps the closed
`work_class` enum on every event, so every call site passes `Some`:
`crate::cost_unit::resource_payload` (`{"work_class": ..., "cost_unit": ...}`) for a
successfully-resolved dispatch, or `crate::cost_unit::base_resource_payload` (`{"work_class":
...}`, no `cost_unit` key) for denied calls, errored dispatches, and the no-pack-owns-this-verb
case. `None` is reserved for a caller with no `work_class` to stamp at all (none exist today); it
must never be used to omit `cost_unit` alone.

## LinkAuditSuccessV2

Schema v2 audit payload for a successful singleton `link` call. Additive over the v1 `AuditEvent`
shape: every v1 field is preserved via `#[serde(flatten)]`, and the edge identity/relation/weight
the caller created or resolved are added at the top level.

## link_audit_success_from_result

Extracts the edge fields needed to enrich a successful singleton `link` audit row from the
handler's returned JSON. Returns `None` (rather than a `Result`) on any missing/malformed field —
the caller treats that as "cannot enrich" and falls back to the v1 audit shape instead of failing
the already-succeeded `link` call.

## resolve_explicit_namespace

This is the single chokepoint both `VerbRegistry::dispatch` (single-backend and JSON-form
ingress) and the multi-backend coordinator intercept (`dispatch_via_coordinator_inner` in
`khive-mcp`) call into, so no ingress path can bypass the fail-closed rule by routing around
`dispatch`.
