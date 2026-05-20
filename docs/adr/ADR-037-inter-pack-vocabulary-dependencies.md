# ADR-037: Inter-pack Vocabulary Dependencies

**Status**: accepted  **Date**: 2026-05-19
**Authors**: khive maintainers

## Context

[ADR-025](ADR-025-pack-standard.md) introduced the `Pack` trait as the composition unit. Each
pack declares its note kinds, entity kinds, and edge endpoint rules as `'static` constants. The
runtime merges all loaded packs into a unified vocabulary and validates incoming operations against
that merged set.

Today, vocabulary is entirely self-contained within each pack:

- `kg` declares `concept`, `document`, `dataset`, `project`, `person`, `org` (entities) and five
  note kinds — no references outside itself.
- `gtd` declares `task` (note) and one `EDGE_RULES` entry: `(task, depends_on, task)`. Both
  endpoints are gtd-owned. No reference to kg vocabulary.

This means the two-pack deployment (`KHIVE_PACKS=kg,gtd`) works today without any dependency
declaration, because each pack's vocabulary is self-sufficient.

The problem surfaces when a pack's `EDGE_RULES` or `KindHook` logic references a kind it does
not own. A concrete example: a future `crm` pack that extends the GTD `task` note with a
`depends_on` edge to a KG `concept` entity (e.g. "this task depends on a design concept being
resolved"):

```rust
impl Pack for CrmPack {
    const EDGE_RULES: &[EdgeEndpointRule] = &[
        EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::NoteOfKind("task"),     // owned by gtd
            target: EndpointKind::EntityOfKind("concept"), // owned by kg
        },
    ];
}
```

If `crm` is loaded without `kg`, the runtime starts. The `concept` entity kind is absent from the
merged vocabulary. The first `link(task, concept, depends_on)` call fails with a confusing
"unknown entity kind: concept" message — as if the user made a typo, not as if a required pack is
missing.

More subtly: the dependency is invisible to both the pack author and the operator running
`KHIVE_PACKS=crm,gtd` (without `kg`). There is no declaration, no compile-time signal, and no
clean load-time error. Discovery happens at the first runtime operation that crosses the boundary,
which may happen minutes into an agent session.

This ADR formalises vocabulary dependencies so they are declared explicitly, checked at load
time, and surfaced with a clear diagnostic.

### Scope boundary

This ADR covers **pack-to-pack vocabulary references** only — the case where pack A's static
declarations (`EDGE_RULES`, `NOTE_KINDS`, `ENTITY_KINDS`) or runtime logic reference vocabulary
owned by pack B. It does not cover:

- Pack versioning or semver constraints (deferred; all packs ship in the same workspace at the
  same version today).
- Dynamic plugin loading (deferred; packs are compile-time composition).
- Runtime validation that a given kind actually exists in the merged registry (this already works
  via `VerbRegistry::all_note_kinds()` / `all_entity_kinds()`; the gap is that "which pack is
  missing" is not surfaced).

## Decision

### `Pack::REQUIRES` — declared dependency on other packs by name

Add a single new `const` to the `Pack` trait in `crates/khive-types/src/pack.rs` (line 69):

```rust
pub trait Pack {
    const NAME: &'static str;
    const NOTE_KINDS: &'static [&'static str];
    const ENTITY_KINDS: &'static [&'static str];
    const VERBS: &'static [VerbDef];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];

    /// Other pack names whose vocabulary this pack references.
    ///
    /// The runtime checks that every name in `REQUIRES` appears in the
    /// loaded pack set before any pack is registered. A missing dependency
    /// produces a fatal error at startup — not a silent runtime failure.
    ///
    /// Defaults to empty. Packs whose vocabulary is entirely self-contained
    /// (currently `kg` and `gtd`) leave this unset.
    const REQUIRES: &'static [&'static str] = &[];
}
```

The parallel method on `PackRuntime` (object-safe mirror in `crates/khive-runtime/src/pack.rs`,
line 35):

```rust
pub trait PackRuntime: Send + Sync {
    // ... existing methods
    fn requires(&self) -> &'static [&'static str] { &[] }
}
```

### Load-time dependency check in `VerbRegistryBuilder::build`

Before registering any pack, `build()` performs a two-phase check:

1. **Collect all pack names** in the builder's pack list.
2. **Walk each pack's `requires()`**; for every named dependency, verify it is present in the
   collected names. If not, return a descriptive error and abort startup.

The check runs before any pack dispatch logic executes — a missing dependency is caught at
process startup, not at the first operation that crosses a vocabulary boundary.

Error message contract (stable, referenced by documentation):

```
missing pack dependency: pack 'crm' requires 'kg', but 'kg' is not in the loaded pack set
```

If multiple packs have unmet dependencies, all are reported in a single error before aborting.

### Topological load order

`REQUIRES` also drives the registration order: packs are sorted topologically by their dependency
graph (depth-first, dependencies before dependents) before `register()` is called in `build()`.
This ensures that when the vocabulary merging loop runs, a dependency's `NOTE_KINDS` and
`ENTITY_KINDS` are already in the merged set before the dependent pack's `EDGE_RULES` are
installed.

Cycles are impossible by construction (a pack cannot depend on itself) but are checked explicitly:
if `REQUIRES` forms a cycle among the named packs, `build()` returns an error before any
registration occurs:

```
circular dependency detected among packs: crm → kg → crm
```

Cycles are a pack-authoring error, not a recoverable runtime condition.

### No-op for current packs

`kg` and `gtd` have no cross-pack references. Both keep the default `REQUIRES = &[]`. The change
is additive — no existing pack requires modification to pass the new check.

### `tools/list` introspection

The `VerbRegistry` exposes a `pack_requires(name: &str) -> &'static [&'static str]` method that
returns a given pack's declared dependencies. The existing `tools/list` description generation
(which already surfaces verbs per pack) includes the dependencies field when non-empty, so MCP
clients can inspect the dependency graph without reading source.

## Rationale

### Why `Pack` const rather than a separate registry mechanism?

The `Pack` const keeps the declaration co-located with the vocabulary it describes. A pack author
editing `EDGE_RULES` to reference a foreign kind sees `REQUIRES` on the same struct — the
dependency is declared where the reference lives. A separate registry mechanism (e.g.,
`VerbRegistryBuilder::add_dependency("crm", "kg")`) shifts the declaration to the transport
layer, away from the pack definition that created the reference. That inversion would make the
link invisible to pack authors working in their own crate.

### Why name-based, not type-based?

The `Pack` trait uses `const NAME: &'static str` as the identity. Type-level dependencies
(`const REQUIRES: &'static [TypeId]`) would require a stable `TypeId` for each pack, which is
not `no_std`-compatible and creates cross-crate coupling at the type level — a `crm` crate would
need `khive-pack-kg` as a compile dependency just to reference its type. Name-based matching
lets `crm` declare `REQUIRES = &["kg"]` without a crate dependency; the runtime resolves it.

### Why load-time check over runtime vocabulary introspection?

Runtime introspection (detecting that an `EDGE_RULES` entry references an unregistered kind when
`link()` is first called) would produce the same final error but at a worse moment — after the
binary has started, after an agent session may have begun, and with a message that does not
distinguish "unknown kind" from "missing pack". Load-time makes the failure deterministic,
immediate, and attributable.

### Why topological sort rather than requiring the user to specify a valid order?

Requiring users to specify `KHIVE_PACKS=kg,crm,gtd` (in the right order) is fragile. The valid
orders are not obvious, especially when three or more packs form a chain. Sorting by `REQUIRES`
is O(n²) at startup, runs once, and eliminates an entire class of "it worked yesterday" bugs when
someone adds a pack to the middle of an env var list.

### Why not `REQUIRES` with version constraints?

All packs ship in the same workspace at the same version. There is no case today where
`crm@0.2` needs `kg>=0.3` but `kg@0.2` is acceptable. Semver constraints add parsing complexity
and documentation burden with no operational benefit in the current deployment model. The door is
left open (a future ADR may extend the syntax from `"kg"` to `"kg@>=0.3"`) by keeping
`REQUIRES` as a string slice — the check today is exact-name membership; a future check can
parse the string for an optional version constraint.

## Alternatives Considered

| Alternative                                                       | Pros                       | Cons                                                                                                   | Why rejected                                                                             |
| ----------------------------------------------------------------- | -------------------------- | ------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| **Implicit load order** (status quo)                              | Zero friction              | Dependency is invisible; runtime error message is misleading; discovered at first cross-pack operation | Does not scale beyond two packs; poor operator experience                                |
| **Runtime-only check** (detect missing kind on `link()`)          | No API change to `Pack`    | Error appears mid-session; message doesn't attribute missing pack; doesn't fail fast                   | Accepted risk today because no cross-pack refs exist; unacceptable as packs grow         |
| **Type-level dependencies** (`TypeId` or generic bounds)          | Compile-time guarantee     | Requires crate-level dep between pack crates; breaks `no_std`; polymorphism overhead                   | Crate coupling defeats the purpose of loose pack composition                             |
| **Semver version constraints** (`"kg@>=0.2"`)                     | Future-proof for ecosystem | Parsing complexity; no operational need with single-workspace deployment                               | Out of scope for v0.1; string syntax leaves room to add it later without breaking change |
| **Separate registry call** (`builder.add_dependency("crm","kg")`) | Keeps `Pack` trait lean    | Declaration is separated from the vocabulary reference that created the need; easy to forget           | Co-location of declaration and reference is the stronger invariant                       |

## Consequences

### Positive

- Cross-pack vocabulary references are now a declared contract, not implicit load-order
  assumptions.
- Missing packs produce a clear startup error attributing the specific pack and dependency.
- Topological sort makes `KHIVE_PACKS` order-independent for well-declared packs.
- Future packs with cross-pack references (CRM, calendar, project tracking) have a standard
  mechanism with no boilerplate beyond the const declaration.
- `tools/list` introspection lets agents understand the pack dependency graph at runtime.

### Negative

- `PackRuntime` gains one method (`requires`). Existing first-party packs get the default
  (`&[]`); third-party packs (none exist yet) would need to add the method if they reference
  foreign vocabulary.
- Topological sort adds a O(n²) startup step. With n ≤ 20 packs (the realistic upper bound for
  the foreseeable future), this is immeasurable in practice but is a new code path in `build()`.

### Neutral

- No schema migration needed; vocabulary dependencies are a pure runtime concept.
- The vocabulary introspection check (does the kind exist in the merged registry at link time?)
  remains unchanged — ADR-037 adds the upstream-pack-present check; the downstream kind-exists
  check stays as-is in `validate_edge_relation_endpoints`.

## Implementation

Three files change; all changes are additive.

| File                               | Change                                                                                                                                                                                                                                        |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-types/src/pack.rs`   | Add `const REQUIRES: &'static [&'static str] = &[]` to the `Pack` trait (after `EDGE_RULES`, line 87)                                                                                                                                         |
| `crates/khive-runtime/src/pack.rs` | Add `fn requires(&self) -> &'static [&'static str] { &[] }` to `PackRuntime` (after `edge_rules`, ~line 53); add `pack_requires` lookup to `VerbRegistry`; extend `VerbRegistryBuilder::build` with the dependency check and topological sort |
| `crates/khive-pack-kg/src/lib.rs`  | No-op: `KgPack` gets `const REQUIRES = &[]` (default; explicit for documentation clarity)                                                                                                                                                     |
| `crates/khive-pack-gtd/src/lib.rs` | No-op: `GtdPack` gets `const REQUIRES = &[]` (default; explicit for documentation clarity)                                                                                                                                                    |

`VerbRegistryBuilder::build` collects all pack names, walks each pack's `requires()`, accumulates
any missing-dependency errors, then performs a DFS topological sort (cycle = error) before
registering in dependency-first order.

No new crates. No new migrations. No changes to the MCP tool surface (`request` is unchanged).

## References

- [ADR-025](ADR-025-pack-standard.md): Pack trait — vocabulary merging and `EDGE_RULES`
- [ADR-031](ADR-031-pack-extensible-edge-endpoints.md): Pack-extensible edge endpoints — the
  motivating case where cross-pack kind references first appear
- [ADR-030](ADR-030-kind-hooks.md): Kind hooks — the other code path where a pack may reference
  foreign kinds in `prepare_create` / `after_create`
