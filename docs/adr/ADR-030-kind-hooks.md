# ADR-030: Kind Hooks — Shared CRUD with Per-Kind Specialization

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-025](ADR-025-pack-standard.md) introduced the `Pack` trait: packs declare
note and entity kinds plus verbs, and the runtime merges those vocabularies.
The §Verb routing section explicitly **deferred kind-discriminated routing**,
so the GTD pack ([ADR-026](ADR-026-gtd-pack.md)) chose distinct verb names
(`assign`, `next`, `complete`, …) to avoid collision with kg's `create`,
`list`, etc.

That deferral left two correctness bugs hiding in plain sight:

1. **The kg `create` handler was validating `note_kind` against the static
   `NoteKind` enum** (kg's own closed taxonomy from ADR-019), not against
   `VerbRegistry::all_note_kinds()` (the merged vocabulary). So even with the
   GTD pack loaded, `create(kind="note", note_kind="task", …)` would be
   rejected — kg didn't know "task" was a registered kind in the runtime.
2. **Every new pack needed its own create/list/search verbs** if it wanted
   to surface a new kind. A future "papers" pack with kinds like
   `paper`, `book`, `whitepaper` — purely structural, no lifecycle — would
   still need to author parallel CRUD handlers, duplicating kg's storage
   wiring for no semantic gain.

The first bug is a fix. The second is a design choice: do we make CRUD
uniformly shared, or do we keep the pack-owns-its-verbs model?

This ADR commits to **shared CRUD + per-kind hooks**.

## Decision

### Shared CRUD lives in the kg pack

`create`, `list`, `update`, `delete`, `merge`, `search` stay in `khive-pack-kg`
as the canonical CRUD verbs. They are pack-owned by kg in the verb-routing
sense (kg's pack registers them), but their _implementation_ is now generic
over kinds — they consult the runtime's merged vocabulary and dispatch
per-kind specialization through a hook trait.

### `KindHook` trait

A new trait in `khive-runtime::pack`:

```rust
#[async_trait]
pub trait KindHook: Send + Sync + std::fmt::Debug {
    /// Mutate args before the storage write. Fill defaults, normalize values,
    /// rearrange user-facing fields into the kg-shape expected by the shared
    /// CRUD handler. Returning an error aborts the create call.
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError>;

    /// Fire side effects after a successful storage write — graph edges,
    /// derived observations, etc. Errors are **logged but not propagated**
    /// (the write already happened; failing the call would mislead the
    /// caller). Implementations `tracing::warn!` and return `Ok(())`.
    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: uuid::Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError>;
}
```

### Packs opt in per-kind

`PackRuntime` gains a default method:

```rust
fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> {
    None  // default: no specialization
}
```

A pack returns `Some(hook)` for kinds it owns AND wants to specialize.
Storage-shape kinds (no defaults, no derived data, no side effects) keep the
`None` default and ride pure CRUD.

### Registry lookup

`VerbRegistry::find_kind_hook(kind) -> Option<Arc<dyn KindHook>>` walks
registered packs in registration order; the first pack that owns the kind AND
returns a hook wins. Used by kg's `handle_create` after canonicalizing the
kind discriminator.

### Hybrid kind canonicalization

kg's vocab (`EntityKind`, `NoteKind`) provides **alias normalization** for kg
kinds (`"paper" → "document"`, `"obs" → "observation"`). Foreign-pack kinds
have no aliases and are matched against `VerbRegistry::all_note_kinds()` /
`all_entity_kinds()` literally. Both resolvers (`canonical_entity_kind`,
`canonical_note_kind`) try kg's enum first, fall back to registry membership.

This preserves kg's alias UX while extending validation to the whole merged
vocabulary.

### Dispatch signature change

`PackRuntime::dispatch` now takes the registry as a parameter:

```rust
async fn dispatch(
    &self,
    verb: &str,
    params: Value,
    registry: &VerbRegistry,
) -> Result<Value, RuntimeError>;
```

`VerbRegistry::dispatch` passes `self` through to the chosen pack. Pack
handlers that need cross-pack vocabulary or hook lookup go through the
registry. This is the single breaking change in `PackRuntime`; both
first-party packs (kg, gtd) were updated in the same change.

### `gtd` registers `TaskHook` for the `task` kind

`GtdPack::kind_hook("task")` returns `Some(Arc::new(TaskHook))`.
`TaskHook::prepare_create` normalizes the gtd-flavored input shape
(`title`, `priority`, `status`, `assignee`, `due`, `depends_on`, `tags`)
into kg's `CreateParams` shape (`name`, `content`, `salience`, `properties`).
`TaskHook::after_create` attempts the `depends_on` graph edges — best-effort,
matching gtd `assign`'s existing semantics.

### `gtd`'s lifecycle verbs stay

`assign`, `next`, `complete`, `tasks`, `transition` remain pack-owned in
`GtdPack`. They encode GTD lifecycle semantics that don't belong in central
CRUD: state-machine transitions, status-priority sort, completion timestamps.
`assign` continues to work as a flavored convenience — `assign(title="x")`
and `create(kind="note", note_kind="task", title="x")` are equivalent on the
write side.

## Rationale

### Why hooks, not "every pack reimplements CRUD"

Reimplementing per-pack means:

- Each pack duplicates kg's storage wiring (entity/note CRUD, namespace
  scoping, error handling, etc.) — ~500 LOC per pack just to support kinds.
- Cross-pack composition gets harder: if you want `create(kind="note",
  note_kind="task")` and `create(kind="note", note_kind="paper")` in the
  same session, you can't — each pack would register its own `create`, and
  first-wins routing picks one.
- Plugin authors writing third-party packs face a steep ramp.

Hooks let storage-shape kinds (the common case) ride a tested CRUD path
with zero code, and lifecycle-shape kinds (the rare case) layer in the
specific behavior they need.

### Why JSON `Value` not a typed args struct

Each kind has different shape needs (a `paper` has `authors`, `year`,
`venue`; a `task` has `priority`, `due`, `depends_on`). A typed argument
struct would need an associated type per kind — pushing the type-level
work back into every hook anyway. `Value` is the lowest-friction shape
the DSL ([ADR-020](ADR-020-request-dsl.md)) already produces.

### Why `after_create` errors are logged not propagated

The storage write succeeded. Returning an error to the caller after a
successful write is misleading: "your task was created but also it failed."
The hook _intends_ side-effect semantics — a missing graph edge is a degraded
result, not a failure. `tracing::warn!` gives operators a signal; the response
to the caller reflects what actually persisted.

The `assign` handler in gtd already had this contract for `depends_on` edges;
the hook preserves it.

### Why hybrid canonicalization (kg enum first, registry fallback)

Pure registry membership would lose kg's alias normalization
(`"paper" → "document"`, `"obs" → "observation"`) — a usability regression.
Pure enum lookup would never see foreign-pack kinds. Hybrid: kg's enum runs
first for its own kinds, registry covers everything else. New packs can
register their own aliases later if a real workflow demands it.

### Why kg owns the shared CRUD

Putting `create` in the runtime would force the runtime to know about
"discriminator validation", "hook dispatch", and the JSON shape of CRUD
ops — all of which are MCP-shaped concerns. Keeping it in the kg pack lets
the runtime stay shape-agnostic: it provides storage, query, hooks; transport
concerns stay in packs. The "kg pack" name is now slightly misleading
(it carries the CRUD verbs for _all_ kinds), but renaming it would churn
ADRs without clarifying anything.

## Alternatives Considered

| Alternative                                       | Pros                                      | Cons                                                                                               | Why rejected                                                        |
| ------------------------------------------------- | ----------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| Per-pack CRUD (status quo from ADR-026)           | Pack autonomy; no cross-pack coupling     | Every new pack reimplements ~500 LOC of CRUD; kg's `create` rejects gtd's `task` kind              | The validation bug alone forces a fix; reimplementation is wasteful |
| Kind-routed dispatch (the deferred plan)          | No hooks needed; each pack owns its verbs | Forces every pack to author full create/list/etc.; first-wins routing doesn't compose              | Same authoring cost as per-pack CRUD                                |
| Single typed associated arg struct per kind       | Compile-time shape safety                 | Associated types complicate the trait; each hook still defines its own struct → same authoring     | Tested at the JSON boundary anyway                                  |
| Runtime owns CRUD; packs are vocabulary-only      | Cleanest layering                         | Runtime grows MCP-shaped knowledge (JSON, discriminator validation, hook dispatch); harder to test | Keeping CRUD in kg keeps the runtime transport-agnostic             |
| `after_create` errors propagate                   | "Atomic" appearance                       | Misleads caller about what persisted; storage write is already committed                           | Best-effort semantics match `assign`'s existing contract            |
| No alias normalization (pure registry membership) | Simpler resolver; one source of truth     | Loses kg's `"paper" → "document"` UX; existing tests break                                         | Hybrid keeps kg's UX while extending validation                     |

## Consequences

### Positive

- The kg-rejects-registered-`task`-kind bug is fixed. `create(kind="note",
  note_kind="task", …)` works end-to-end through the shared CRUD path.
- Future packs with purely structural kinds (e.g. a "papers" pack adding
  `paper`/`book`/`whitepaper`) need zero CRUD code — declare kinds, optionally
  register a hook for defaults, done.
- gtd's `assign` and shared `create(…, note_kind="task")` produce equivalent
  task notes. Two valid paths to the same outcome; agents pick whichever fits
  their context.
- The single dispatch site established by ADR-027 (one `request` tool) becomes
  the natural place to wire authorization (ADR-029) + hooks — one choke point
  for cross-cutting concerns.

### Negative

- `PackRuntime::dispatch`'s signature changed (added `registry: &VerbRegistry`
  parameter). Breaking for external packs (none exist yet); first-party packs
  updated in the same change.
- Hooks operate on `serde_json::Value`, not a typed struct — type errors land
  at runtime, not compile time. Mitigated by integration tests at the pack
  level.
- The kg pack now owns CRUD for _all_ kinds, not just its own. The crate name
  is slightly misleading.
- Per-create `params.clone()` cost in `handle_create` to keep the args
  available for `after_create`. JSON values are small for typical verb args
  (~100 bytes); profiled cost is negligible.

### Neutral

- gtd's lifecycle verbs (`assign`, `complete`, `transition`, `next`, `tasks`)
  stay pack-owned. The hook pattern handles create-time specialization; it
  does not (yet) cover update or delete hooks. Extending to those is a
  follow-up if a real workflow needs it.
- The `EntityKind` and `NoteKind` enums in kg's `vocab` module stay
  authoritative for kg's own taxonomy + aliases. They're no longer
  _exclusive_ — they're the first lookup in the hybrid resolver.

## Implementation Status

| Step                                                                       | Where                                             | Status |
| -------------------------------------------------------------------------- | ------------------------------------------------- | ------ |
| `KindHook` trait + `kind_hook` method on `PackRuntime`                     | `crates/khive-runtime/src/pack.rs`                | done   |
| `VerbRegistry::find_kind_hook`                                             | `crates/khive-runtime/src/pack.rs`                | done   |
| `PackRuntime::dispatch` accepts `&VerbRegistry`                            | `crates/khive-runtime/src/pack.rs` + both packs   | done   |
| Hybrid kind canonicalization helpers                                       | `crates/khive-pack-kg/src/handlers.rs`            | done   |
| kg `handle_create` rewritten with hook lookup + registry validation        | `crates/khive-pack-kg/src/handlers.rs`            | done   |
| kg `handle_list` validation extended to registry vocabulary                | `crates/khive-pack-kg/src/handlers.rs`            | done   |
| `TaskHook` impl in gtd                                                     | `crates/khive-pack-gtd/src/hook.rs` (new file)    | done   |
| `GtdPack::kind_hook("task")` returns `TaskHook`                            | `crates/khive-pack-gtd/src/lib.rs`                | done   |
| Unit tests for `find_kind_hook`                                            | `crates/khive-runtime/src/pack.rs` (3 tests)      | done   |
| Integration tests: shared CRUD path produces task notes; vocab error lists | `crates/khive-mcp/tests/integration.rs` (3 tests) | done   |

## Open Questions

1. **Update / delete hooks.** Should `KindHook` extend to `prepare_update` /
   `after_update` / `before_delete`? Gtd's `complete` and `transition` cover
   the lifecycle update case via custom verbs; structural-only kinds may not
   need update hooks at all. Defer until a real consumer asks.
2. **Hook composition.** If two packs both register a hook for the same kind,
   first-wins routing picks one. Is that right? Could imagine middleware-style
   chaining (each hook wraps the inner one). Defer until a real second-pack
   case emerges.
3. **Should kg's CRUD move to the runtime?** Now that kg's `create` is
   generic over kinds, the kg-pack-owns-it boundary is mostly cosmetic. A
   future refactor could lift it to `khive-runtime::operations::shared_crud`
   and have kg just register the verb names. Not urgent.
4. **Deprecate `assign`?** With `create(kind="note", note_kind="task")`
   working, `assign` is redundant on the write side. It still has unique
   value (gtd-flavored arg shape; user-friendly verb name) so keep it. Mark
   the equivalence in the gtd plugin's SKILL.md so agents know either works.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): 6 entity kinds (kg's vocab)
- [ADR-019](ADR-019-note-kind-taxonomy.md): 5 note kinds (kg's vocab)
- [ADR-025](ADR-025-pack-standard.md): `Pack` trait — vocabulary merging
- [ADR-026](ADR-026-gtd-pack.md): GTD pack — the lifecycle-shape sibling
- [ADR-027](ADR-027-single-tool-mcp-surface.md): single dispatch site
- [ADR-029](ADR-029-authorization-gate.md): the gate consulted at the same
  dispatch site
