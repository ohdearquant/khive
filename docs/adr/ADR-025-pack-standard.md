# ADR-025: Pack Standard — Composable Vocabulary Extension

**Status**: accepted\
**Date**: 2026-05-17\
**Authors**: Ocean

## Context

khive's closed taxonomies — 6 entity kinds (ADR-001), 5 note kinds (ADR-019), 13 edge relations
(ADR-021) — serve the KG use case well. The closed sets prevent vocabulary drift and give agents
unambiguous classification targets.

However, downstream deployments and third-party plugins may need additional kinds beyond the base
KG set. For example, a task-management plugin might need `task` and `message` note kinds; a
hardware-inventory plugin might need `device` entity kinds. These are not KG concerns — adding them
to ADR-001/ADR-019 would pollute the KG taxonomy and break the classification invariants those ADRs
establish.

The current compile-time enums in `khive-types` prevent extension without forking:

1. Any new kind requires an ADR amendment and a code change to the enum.
2. There is no mechanism for a plugin to introduce kinds without modifying core types.
3. The closed-set discipline is the right approach for edge relations (graph semantics are universal)
   but is too rigid for note kinds and entity kinds, which are domain-classification concerns.

This ADR introduces a composition mechanism that preserves closed-set discipline within a pack while
allowing the runtime to merge vocabularies from multiple packs.

## Decision

### Pack trait

Introduce a `Pack` trait in `khive-types` as the universal composition unit. Each pack declares
vocabulary (note kinds, entity kinds) via const associated items. The trait lives in `khive-types`
(no_std, zero dependencies) so anything that validates kinds can depend only on types, not the full
runtime.

```rust
// crates/khive-types/src/pack.rs
pub struct VerbDef {
    pub name: &'static str,
    pub description: &'static str,
}

pub trait Pack {
    /// Short identifier for this pack (e.g. "kg", "tasks", "inventory").
    const NAME: &'static str;

    /// Note kinds this pack contributes to the runtime vocabulary.
    ///
    /// Validated at the service boundary — creating a note with a kind not registered
    /// by any loaded pack is rejected with the full valid list.
    const NOTE_KINDS: &'static [&'static str];

    /// Entity kinds this pack contributes to the runtime vocabulary.
    ///
    /// Same validation semantics as note kinds.
    const ENTITY_KINDS: &'static [&'static str];

    /// Verbs this pack handles. The runtime routes verb calls to the pack
    /// that declares them.
    const VERBS: &'static [VerbDef];
}
```

Const associated items (`&'static str` and `&'static [&'static str]`) require no heap allocation
and are compatible with `#![no_std]`.

### PackRuntime — object-safe dispatch trait

A `PackRuntime` trait in `khive-runtime` provides async verb dispatch. It does NOT live in
`khive-types` — behavior requires the full runtime context.

`Pack` uses const associated items (`const NAME`, `const VERBS`, etc.) which are not object-safe
in Rust — you cannot use `dyn Pack` or `Box<dyn Pack>`. Since the registry needs to store
heterogeneous packs, `PackRuntime` mirrors the `Pack` metadata as methods and adds dispatch:

```rust
// crates/khive-runtime/src/pack.rs
#[async_trait]
pub trait PackRuntime: Send + Sync {
    fn name(&self) -> &str;
    fn note_kinds(&self) -> &'static [&'static str];
    fn entity_kinds(&self) -> &'static [&'static str];
    fn verbs(&self) -> &'static [VerbDef];
    async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError>;
}
```

Implementors must also implement `Pack` on the same struct — the methods must return the same
values as the corresponding consts. This is enforced by convention and tests, not the type system,
because Rust's object safety rules prohibit const items in trait objects.

### Runtime vocabulary merging

At init the runtime collects `NOTE_KINDS` and `ENTITY_KINDS` from all loaded packs into a merged
set. Any `create` or `update` call with an unregistered kind returns an error listing all valid
values from all loaded packs. The merge is additive — packs can overlap (two packs declaring the
same kind string is not an error; it is idempotent in the merged set).

### Verb routing: name-based vs kind-discriminated

Verbs fall into two categories:

1. **Pack-specific verbs** — verbs unique to one pack (e.g. a hypothetical `reindex` or
   `schedule`). Routed by verb name alone: the registry dispatches to the pack that declares it.

2. **Shared CRUD verbs** — verbs like `create`, `list`, `search`, `get`, `update`, `delete` that
   multiple packs handle. These require **kind-discriminated routing**: the registry inspects the
   `kind` / `entity_kind` / `note_kind` parameter to determine which pack owns that vocabulary
   entry, then dispatches to that pack.

Kind-discriminated routing is implemented in step 5 (MCP rewrite). Until then, only one pack is
loaded (the `kg` pack) and the distinction is moot — verb-name dispatch is sufficient for the
single-pack case. The `VerbRegistry` already provides `all_note_kinds()` and `all_entity_kinds()`
which are the building blocks for kind-discriminated dispatch.

### Wire types

Entity and note kinds on the wire stay `String`. Validation is runtime, not compile-time. This is a
deliberate relaxation from the compile-time enum approach — the enum approach works when the closed
set is fixed at compile time; it does not work when the valid set is determined by which packs are
loaded.

### Edge relations stay closed

`EdgeRelation` remains a closed enum (ADR-021). Edge relations define graph semantics — their
meaning is universal across all packs. A `contains` edge means the same thing regardless of which
pack the endpoints belong to. Packs cannot add edge relations.

### Built-in pack

| Pack | Note kinds                                          | Entity kinds                                     | Location          |
| ---- | --------------------------------------------------- | ------------------------------------------------ | ----------------- |
| kg   | observation, insight, question, decision, reference | concept, document, dataset, project, person, org | `khive-pack-kg`   |

The `kg` pack is the only pack shipped in the OSS distribution. It is registered into the
`VerbRegistry` by the transport layer (step 5). Extension packs are separate crates that implement
the `Pack` + `PackRuntime` traits and register alongside `kg` at init.

## Rationale

### Why const associated items for `Pack` (in khive-types)?

For the declarative metadata layer, methods would require vtable dispatch and heap allocation.
Const items are zero-cost and enable static initialization. They also prevent accidental state:
vocabulary is a static declaration, not a runtime computation.

The runtime dispatch layer (`PackRuntime` in khive-runtime) intentionally uses object-safe methods
because it needs heterogeneous storage (`Box<dyn PackRuntime>`). The `Pack` consts remain the
source of truth; `PackRuntime` methods mirror them for object-safety.

### Why `&'static [&'static str]` instead of a vocabulary enum?

An enum would re-introduce the problem: adding a kind requires changing the enum. Static string
slices are the minimal representation that satisfies `no_std`, avoids allocation, and lets each
pack own its vocabulary without a shared discriminant type.

### Why not feature-gated enums?

Feature gates would require consumers to know which packs are loaded at compile time and rebuild
when the pack set changes. Runtime composition is strictly more flexible — a single binary can
serve multiple pack configurations without recompilation.

### Why not "any string, no validation"?

Removing validation entirely would re-introduce the vocabulary drift problem that ADR-001 and
ADR-019 solved. The goal is composable discipline, not no discipline. Each pack is internally
closed (its vocabulary is a compile-time constant); the runtime enforces the merged set.

### Why not dynamic loading via trait objects?

Dynamic loading (dlopen, plugin .so) introduces linking complexity, version skew risk, and
security surface. The pack model targets compile-time composition — packs are linked into the
binary, not loaded at runtime. Dynamic loading is a separate concern and is explicitly deferred.

## Alternatives Considered

| Alternative                         | Pros                            | Cons                                                                      | Why rejected                                                  |
| ----------------------------------- | ------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------- |
| Feature-gated enums                 | Compile-time exhaustiveness     | Requires rebuild per pack set; no runtime composition                     | Too inflexible for multi-configuration deployment             |
| No validation (any string accepted) | Zero friction for new kinds     | Vocabulary drift; agents can't discover valid kinds; inconsistent storage | Same failure mode ADR-001 and ADR-019 fixed                   |
| Trait objects for `Pack` metadata   | True runtime extensibility      | Heap allocation required; not no_std compatible in types layer            | Rejected for types layer; accepted for runtime dispatch layer |
| Dynamic library loading             | Truly separate plugin artifacts | Security surface; version skew; linking complexity                        | Deferred; compile-time composition covers all known use cases |
| Single shared enum across all packs | Compile-time exhaustiveness     | Pollutes KG taxonomy; breaks ADR-001/ADR-019 invariants                   | Mixing domain concerns breaks classification discipline       |

## Consequences

### Positive

- `khive-types` stays stable: the OSS public API surface does not change with each extension pack.
- Packs are self-contained: a pack declares its vocabulary in one place, with no changes to any
  shared enum.
- The same composition mechanism covers all use cases (KG, task management, custom domains).
- The `kg` pack is the default — the OSS runtime behavior is unchanged for users who load no
  additional packs.
- `no_std` compatibility is preserved in `khive-types`.

### Negative

- No compile-time exhaustiveness for kind matching in code that handles arbitrary packs. Code that
  needs to switch on kind must either use a `match` with a fallback arm or look up the kind string
  dynamically.
- Kind strings must be coordinated across packs to avoid semantic collisions (two packs declaring
  "task" with different semantics). Mitigation: the pack registry (tracked as a future ADR) will
  document canonical kind strings and flag collisions.
- Validation happens at the service boundary, not at the call site. Invalid kinds surface as
  runtime errors rather than compile errors. Acceptable: the pack set is not knowable at compile
  time.

## Implementation Status

This ADR is implemented incrementally across multiple PRs:

| Step                                                                     | Description                    | Status  |
| ------------------------------------------------------------------------ | ------------------------------ | ------- |
| 1. Pack trait + VerbDef in `khive-types`                                 | Declarative metadata (this PR) | done    |
| 2. PackRuntime trait + VerbRegistry in `khive-runtime`                   | Async dispatch layer           | done    |
| 3. Strip fixed `EntityKind`/`NoteKind` validation from runtime and query | Make runtime pack-agnostic     | done    |
| 4. `khive-pack-kg` crate with vocabulary and verb handlers               | First concrete pack            | done    |
| 5. Rewrite `khive-mcp` to route through VerbRegistry                     | Single `request` tool surface  | pending |

Steps 1-4 establish the single-pack architecture: the `kg` pack is the only pack loaded, so
verb-name dispatch is sufficient. Step 5 adds kind-discriminated routing to the MCP layer,
enabling multi-pack deployment where shared CRUD verbs dispatch based on the kind parameter
rather than just the verb name.

## References

- ADR-001: Entity Kind Taxonomy (6 KG entity kinds; `kg` pack encodes these)
- ADR-019: Note Kind Taxonomy (5 KG note kinds; `kg` pack encodes these)
- ADR-021: EdgeRelation Enum (edge relations stay a closed enum — Pack does not extend them)
