# Pack lifecycle: warm-up and validator installation

Technical reference for `KgPack::warm` and its entity-type validator installation, run at
daemon construction / first pack install (`dispatch.rs`).

## `warm()` telemetry (ADR-103 Amendment 1 Part 2)

`warm()` runs at daemon construction / first pack install, outside `dispatch()` entirely —
there is no caller-supplied token. It mints one the same way `khive-pack-memory`'s ANN
background-rebuild task does (`rt.authorize(Namespace::local())`), so this daemon-startup
embedder warmup is attributed to the daemon principal instead of remaining invisible on the
event plane. A mint failure only removes this pass's telemetry — the warmup itself still
runs.

Every `warm()` call emits exactly one `PhaseStarted` and one terminal event
(`PhaseCompleted`/`PhaseCancelled`) — this phase-span contract is regression-covered
end to end.

## Entity-type validator installation

Installs the validator on the runtime this pack OWNS, not on the caller-supplied runtime. In
a multi-backend deployment the pack is constructed with a per-pack runtime (see
`PackRegistry::register_packs_with_runtimes`); `self.runtime` is that runtime. In a
single-backend deployment `self.runtime` IS the single runtime, so behavior is identical to
the previous call-through. The validator is composed once from every loaded pack's
`ENTITY_TYPES` (`VerbRegistry::all_entity_types`, threaded in by
`call_register_entity_type_validators`) layered over the builtin registry, so pack-declared
subtypes (e.g. git's `adr` Document subtype) validate here in addition to
`EntityTypeRegistry::global()`'s builtin-only set. See
[entity-kind-validation.md](entity-kind-validation.md) for the validator's own contract.
