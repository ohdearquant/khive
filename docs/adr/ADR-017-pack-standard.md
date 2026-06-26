# ADR-017: Pack Standard

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

> **Amended ([ADR-055](ADR-055-epistemic-edge-relations.md))**: ADR-055 added 2
> epistemic relations (`supports`, `refutes`), expanding the closed set from 15 to 17.
> Occurrences of "15 edge relations" in this document reflect the original base set.

## Context

khive's foundational substrate (ADR-001 through ADR-016) is closed and stable. Six
entity kinds (ADR-001 — Concept, Document, Dataset, Project, Person, Org, plus the
two added later) plus five base note kinds (ADR-013 — observation, insight, question,
decision, reference) plus fifteen edge relations (ADR-002) plus eight storage
capabilities (ADR-005) form the foundation.

But foundation is not the entire ecosystem. A task-tracking deployment needs `task`
notes with lifecycle. A memory-augmented agent needs `memory` notes with decay-weighted
recall. A research pipeline needs typed `artifact` entities with provenance edges. A
calendar pack needs `event` notes with temporal logic.

The system must satisfy:

1. **Closed core, open extension.** The core taxonomies stay closed (vocabulary drift
   is prevented). Packs add their own kinds, verbs, and endpoint rules without forking
   foundational crates.
2. **One composition mechanism for everything.** Vocabulary, verbs, kind-specific logic,
   edge endpoints, storage, and configuration all flow through one pack abstraction.
3. **Pack autonomy with runtime coherence.** Each pack owns its concerns. The runtime
   merges packs' contributions into one coherent surface at boot time. Collisions are
   boot-time errors, not runtime mysteries.
4. **No special-casing of built-in packs.** The kg pack is a pack — it uses the same
   trait, registers the same way, and could in principle be replaced. Foundations of
   architecture, not foundations of code privilege.
5. **`no_std`-compatible declarations.** Pack metadata (vocabulary, verbs, endpoint
   rules) lives in `khive-types` as const associated items, requiring no allocator. The
   runtime dispatch layer (`PackRuntime`) lives in `khive-runtime` with full async.

## Decision

### The `Pack` trait: declarative metadata

`Pack` lives in `khive-types` with const associated items only — no methods, no
allocation, no async.

```rust
// crates/khive-types/src/pack.rs
pub enum Visibility {
    Verb,
    Subhandler,
}

pub struct HandlerDef {
    pub name: &'static str,
    pub visibility: Visibility,
    // schemas, speech-act class, auth metadata, etc.
}

pub enum EndpointKind {
    NoteOfKind(&'static str),
    EntityOfKind(&'static str),
    // Match a granular entity SUBTYPE (the `entity_type` property), bound to its
    // base entity kind. A subtype rule MUST carry both: the matcher requires
    // base-kind == `kind` AND entity_type == `entity_type`. `EntityOfKind` alone
    // sees only the base kind, so it is inert for subtype targeting.
    EntityOfType { kind: &'static str, entity_type: &'static str },
}

pub struct EdgeEndpointRule {
    pub relation: EdgeRelation,
    pub source: EndpointKind,
    pub target: EndpointKind,
}

pub trait Pack {
    /// Short identifier (e.g., "kg", "gtd", "memory").
    const NAME: &'static str;

    /// Note kinds this pack registers. Validated against the merged set at boot.
    const NOTE_KINDS: &'static [&'static str];

    /// Entity kinds this pack registers. Validated against the merged set at boot.
    const ENTITY_KINDS: &'static [&'static str];

    /// Handlers this pack registers. Boot-time verb-name collisions across packs
    /// are errors. Only entries with `visibility: Visibility::Verb` are surfaced
    /// on the MCP wire; `Visibility::Subhandler` entries are internal.
    const HANDLERS: &'static [HandlerDef];

    /// Edge endpoint rules this pack contributes. Additive to ADR-002's base contract.
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];

    /// Other pack names whose vocabulary this pack references.
    ///
    /// The runtime verifies every name in `REQUIRES` is present in the loaded pack
    /// set before any pack is registered. A missing dependency aborts startup with
    /// an attributable error ("pack X requires Y, but Y is not loaded"). The
    /// runtime also topologically sorts packs by this graph so dependencies
    /// register before dependents.
    ///
    /// Defaults to empty. Packs whose vocabulary is entirely self-contained leave
    /// this unset.
    const REQUIRES: &'static [&'static str] = &[];

    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }

    fn verbs(&self) -> impl Iterator<Item = &'static HandlerDef> {
        Self::HANDLERS
            .iter()
            .filter(|h| matches!(h.visibility, Visibility::Verb))
    }
}
```

`HandlerDef` carries `visibility: Visibility`. `Visibility::Verb` is externally invokable;
`Visibility::Subhandler` is internal (e.g., `memory.recall_score` is a subhandler —
addressable through batch DSL chains but NOT registered as a top-level MCP verb).

`Pack` is a declaration interface. It says what the pack contributes. It does not
do anything. Each const can be evaluated at compile time and serves as the source of
truth for the pack's metadata.

### `PackRuntime`: object-safe dispatch

Const associated items prevent `Pack` from being `dyn`-compatible. `PackRuntime` is the
runtime mirror — methods that the registry calls through trait objects.

```rust
// crates/khive-runtime/src/pack.rs
#[async_trait]
pub trait PackRuntime: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &str;
    fn note_kinds(&self) -> &'static [&'static str];
    fn entity_kinds(&self) -> &'static [&'static str];
    fn handlers(&self) -> &'static [HandlerDef];
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] { &[] }

    /// Mirror of `Pack::REQUIRES`. Object-safe alternative for dynamic packs
    /// (e.g., `DeclarativePack` per ADR-023) that compute their dependencies at
    /// runtime rather than at compile time.
    fn requires(&self) -> Vec<String> { Vec::new() }

    /// Storage profile — placement roles, default backend (ADR-003, ADR-015).
    fn storage_profile(&self) -> StorageProfile;

    /// Pack-auxiliary schema applied at boot via CREATE TABLE IF NOT EXISTS
    /// (ADR-015). Idempotent. Pack tables are non-versioned in v1.
    fn schema_plan(&self) -> SchemaPlan { SchemaPlan::empty() }

    /// Per-kind specialization hook. Default `None` — kinds use shared CRUD with
    /// no specialization.
    fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> { None }

    /// Dispatch a verb registered by this pack.
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError>;
}
```

Implementors must also implement `Pack` on the same struct. The metadata methods on
`PackRuntime` return the same values as the corresponding `Pack` consts. Enforcement
is by convention; const items cannot live in `dyn` traits in Rust.

### `KindHook`: per-kind specialization for shared CRUD

The kg pack owns the canonical CRUD verbs (`create`, `list`, `update`, `delete`,
`merge`, `search`). These verbs are generic over kind — they consult the runtime's
merged vocabulary and dispatch per-kind specialization through `KindHook`.

```rust
// crates/khive-runtime/src/pack.rs
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
    /// derived observations, etc. Errors are logged but not propagated
    /// (the write already happened; failing the call would mislead the
    /// caller). Implementations `tracing::warn!` and return `Ok(())`.
    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError>;
}
```

A pack registers a hook for kinds it wants to specialize. Storage-shape kinds
(no defaults, no derived data, no side effects) skip the hook entirely and ride pure
shared CRUD.

`KindHook` is one of many possible specialization points. Future hooks
(`prepare_update`, `after_update`, `before_delete`) extend the pattern. v1 ships
`prepare_create` + `after_create` because those cover the common case of "fill
defaults, fire side effects on creation."

### `VerbRegistry`: the runtime's pack catalog

`VerbRegistry` is the runtime's authoritative view of loaded packs. The binary
(kkernel) constructs it at startup, registers each pack, and freezes it.

```rust
// crates/khive-runtime/src/pack.rs
pub struct VerbRegistry {
    packs: Vec<Arc<dyn PackRuntime>>,
    default_namespace: Namespace,
    gate: Arc<dyn Gate>,                            // ADR-018
    event_store: Option<Arc<dyn EventStore>>,       // ADR-018 audit persistence
}

impl VerbRegistry {
    pub fn builder() -> VerbRegistryBuilder { ... }

    /// All registered note kinds, deduplicated.
    pub fn all_note_kinds(&self) -> Vec<&'static str>;

    /// All registered entity kinds, deduplicated.
    pub fn all_entity_kinds(&self) -> Vec<&'static str>;

    /// All registered handlers with their owning pack.
    pub fn all_handlers(&self) -> Vec<(HandlerDef, &str)>;

    /// All MCP-exposed verbs (visibility == Verb) with their owning pack.
    pub fn all_verbs(&self) -> Vec<(HandlerDef, &str)>;

    /// All edge endpoint rules from all packs.
    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule>;

    /// Find a kind hook by kind name. First pack that registers a hook wins.
    pub fn find_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>>;

    /// Find the pack that owns a verb.
    pub fn find_verb_owner(&self, verb: &str) -> Option<Arc<dyn PackRuntime>>;

    /// Dispatch a parsed op (ADR-016 ParsedOp) to the appropriate pack handler.
    pub async fn dispatch(
        &self,
        op: ParsedOp,
        token: NamespaceToken,
    ) -> Result<Value, RuntimeError>;
}
```

### `PackEventConsumer`: event delivery contract

Packs that derive state from the event log (brain, audit summary, future analytics)
register as event consumers via the `PackEventConsumer` trait. The runtime delivers
matching events one at a time; the pack owns reduction, persistence, and replay.

```rust
pub trait PackEventConsumer: Send + Sync {
    /// Predicate evaluated against each emitted event. EventFilter is the canonical
    /// SQL-executable predicate (ADR-022 §3a). Returning an empty/match-all filter
    /// causes the consumer to receive every event.
    fn event_filter(&self) -> EventFilter;

    /// Invoked once per matching event, in canonical replay order
    /// (created_at ASC, event_id ASC — ADR-022 §3b). The consumer receives an
    /// `EventView` (ADR-041 §5) — the raw event plus its pre-joined provenance
    /// observations. The consumer is responsible for loading state, applying its
    /// Fold::reduce against the right field of the view (see ADR-041 §5 for the
    /// two call shapes), and persisting state + cursor atomically.
    async fn on_event(
        &self,
        view: &EventView,
        ctx: &RuntimeEventContext,
    ) -> RuntimeResult<()>;
}

pub struct RuntimeEventContext {
    pub namespace: String,
    pub event_cursor: EventCursor,   // see ADR-022 §3b
    // future: tracing span, deadline, cancellation token
}
```

#### State + cursor atomicity (MANDATORY)

A pack consumer MUST persist its derived state and the `EventCursor` of the event it
just processed in the **same logical transaction**. A crash between state-update and
cursor-update causes duplicate reduction on restart, breaking replay determinism for
non-idempotent Folds.

```rust
// Inside on_event(view: &EventView, ctx):
let mut tx = pack.storage.begin().await?;
let mut state = tx.load_state(profile_id).await?;
// For Fold<Event, S> impls, pass the raw event:
state = fold.reduce(state, &view.event, &fold_ctx);
// (For Fold<EventView, S> impls — see ADR-041 §5 — pass `view` instead.)
tx.save_state(profile_id, &state).await?;
tx.save_cursor(profile_id, &EventCursor::from(&view.event)).await?;
tx.commit().await?;
```

The atomicity guarantee spans three tables in total: `events` and
`event_observations` (written by the runtime dispatcher in its own transaction
before `on_event` fires — ADR-041), plus the pack-owned `(state, cursor)` pair
written here. The pack's transactional obligation is ONLY the latter pair; the
dispatcher commits the event + observation rows before invoking `on_event`, so a
crash inside the consumer leaves a fully-written `EventView` upstream and a
cleanly absent (or stale) state/cursor downstream — replay recovers correctly.

State and cursor MUST live in the same backend. Packs that use pack-scoped backends
(ADR-028) must keep state and cursor co-located in the pack's primary backend;
cross-backend state/cursor split is out of scope (the WAL pattern in ADR-029 is for
hard-delete cascade, a different invariant — do not reuse for replay continuity).

#### Catch-up on registration / restart

When a consumer registers or restarts:

1. Load the last persisted `EventCursor` (or use `EventCursor::zero()` for cold start).
2. Query the event store with the §3b ascending replay query and the persisted cursor.
3. Invoke `on_event` for each matching event in order; persist state + cursor each
   step (or batch into one transaction every N events as an optimization — but never
   batch across events whose `created_at` differs from the persisted cursor without
   re-reading on resume).
4. Once caught up, switch to live mode — the runtime delivers each newly-appended
   matching event via `on_event` directly.

The runtime does NOT persist Fold state. The runtime does NOT execute Folds. It
delivers events and invokes `on_event` — everything downstream is pack territory.

#### Failure isolation

If a pack consumer's `on_event` returns an error, the runtime:

1. Logs the error with `(pack_id, profile_id, event_cursor)`.
2. Does NOT re-deliver the same event automatically (avoids tight retry loops).
3. Does NOT abort event append for other consumers — the event log is the source of
   truth and survives individual consumer failures.
4. Leaves the consumer's cursor at its last successful position; the next live event
   that matches the filter triggers a catch-up that re-attempts the failed event.

Operators may force re-processing via `kkernel events replay --pack <id> --from
<cursor>` (CLI, not MCP — see ADR-032 §11 / concern-6 Q6.5).

### Boot-time collision checks

The registry rejects:

1. **Two packs registering the same `Visibility::Verb` handler name.** `BootError::VerbCollision { verb,
   first_pack, second_pack }`. `Visibility::Subhandler` entries are namespaced under the
   pack and do not participate in cross-pack collision checks.
2. **A pack registering a verb name that the parser cannot accept.** Verb names follow
   `[a-z][a-z0-9_]*` and are at most 32 characters.
3. **A pack-registered kind that violates kind-name rules.** Kind names follow the same
   `[a-z][a-z0-9_]*` pattern.
4. **A pack rule whose `source` or `target` references a kind not registered by any
   loaded pack.** `BootError::UnknownKindInRule { rule, kind }`. This prevents dead
   rules.
5. **A pack whose `REQUIRES` names a pack not in the loaded set.**
   `BootError::MissingDependency { pack, requires }`. All missing dependencies across
   the loaded set are reported in a single error before aborting.
6. **A `REQUIRES` graph containing a cycle.** `BootError::DependencyCycle { cycle:
   Vec<&'static str> }`. Cycles are a pack-authoring error; the runtime cannot recover.

Kind name collisions across packs are NOT errors — two packs declaring the same kind
string (e.g., `task`) are idempotent in the merged vocabulary set. The runtime
de-duplicates for schema display purposes. However, when multiple **pack instances**
declare ownership of the same granular kind (e.g., two `memory` pack instances), the
registry preserves per-instance routing internally. See "Pack instance kind collision"
below.
Semantic collisions are documented through the pack-registry conventions, not enforced
in code.

### Pack instance kind collision (multiple instances declaring same granular kind)

```rust
pub struct KindRoute {
    pub kind: KindName,
    /// All pack instances that declare this kind and support read.
    pub readable_instances: Vec<PackInstanceId>,
    /// The operator-declared primary write target.
    /// REQUIRED when more than one writable instance owns this kind —
    /// registration-order auto-selection is forbidden.
    pub primary_write_instance: PackInstanceId,
    /// Pack instances that can be explicitly targeted via `instance=` override.
    pub explicit_instances: Vec<PackInstanceId>,
    /// Fusion strategy used when reads fan out across instances.
    pub read_fusion: BackendFusionStrategy,
}
```

When two or more pack instances declare the same granular kind (e.g., `memory-hot` and
`memory-cold` both declare `kind=memory`), the registry preserves all owners:

- **Reads fan out** across all `readable_instances`; results fuse using `read_fusion`
  (default: backend-level RRF).
- **Writes route deterministically** to `primary_write_instance`. The operator MUST
  declare this when multiple writable instances own a kind — registration order is NOT
  a valid tiebreaker.
- **Explicit override** is supported via `instance="memory-cold"` arg on writes
  (subject to auth).

Public kind de-duplication is fine for schema display, but the registry MUST retain
per-instance routing internally.

### Inter-pack dependencies

`Pack::REQUIRES` is a declared dependency list referenced by name. Three behaviours
follow from a non-empty `REQUIRES`:

1. **Load-time check.** Before any pack registers, the registry collects all pack
   names, then walks each pack's `REQUIRES` and verifies every name is present.
   Missing dependencies are caught at startup, not at the first cross-pack operation.
2. **Topological registration order.** Packs sort by their dependency graph (DFS,
   dependencies before dependents). A dependency's `NOTE_KINDS`/`ENTITY_KINDS` are in
   the merged set before any dependent pack's `EDGE_RULES` reference them — eliminating
   order sensitivity in the `--pack` flag list or `KHIVE_PACKS` env order.
3. **Diagnostic clarity.** The error
   `"pack 'crm' requires 'kg', but 'kg' is not in the loaded pack set"` attributes the
   failure to the specific pack and its specific missing dependency, instead of the
   misleading `"unknown entity kind: concept"` an agent would see on the first
   cross-pack `link` call without this check.

The default `REQUIRES = &[]` covers packs with self-contained vocabulary (kg, gtd).
Packs whose `EDGE_RULES` or `KindHook` logic references foreign kinds — typical for
domain packs that build on the kg substrate — declare what they need. The mechanism is
name-based rather than type-based to keep `Pack` `no_std`-compatible and to avoid forcing
crate-level dependencies between unrelated pack crates.

Declarative packs ([ADR-023](ADR-023-declarative-pack-format.md)) use the same
mechanism through the `PackRuntime::requires()` object-safe method.

### Vocabulary merging

At boot, the registry aggregates:

```text
all_note_kinds   = ∪ pack.note_kinds()      for pack in registered_packs
all_entity_kinds = ∪ pack.entity_kinds()    for pack in registered_packs
all_handlers     = { (handler, pack) : pack in registered_packs, handler in pack.handlers() }
all_verbs        = { (handler, pack) : handler in all_handlers, handler.visibility == Verb }
all_edge_rules   = ∪ pack.edge_rules()      for pack in registered_packs
all_storage_profiles = { (pack.name, pack.storage_profile()) : pack ∈ packs }
```

`create`, `list`, `search` consult the merged kind sets. Unknown kinds at the verb
boundary return `RuntimeError::UnknownKind { kind, registered: Vec<&'static str> }`
with the full registered set in the error message.

### Pack-extensible edge endpoints

The 15 edge relations (ADR-002) are closed. Their semantics are universal: `depends_on`
means "X cannot complete without Y" regardless of substrate. Packs cannot add relations.

But the per-relation **endpoint contract** (which `(source, relation, target)` triples
are legal) is pack-extensible. A pack declares additional legal endpoint triples for
existing relations:

```rust
// In khive-pack-gtd
const EDGE_RULES: &[EdgeEndpointRule] = &[
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("task"),
        target: EndpointKind::NoteOfKind("task"),
    },
];
```

This makes `link(task_a, task_b, depends_on)` legal in deployments that load `gtd`.
ADR-002's base contract (`depends_on` requires entity→entity) is preserved; the rule
broadens it for task notes.

**Additive only.** A pack cannot tighten ADR-002's base contract. It cannot remove
relations from being legal between two entity kinds. It can only add new legal pairs.

**Subtype endpoints.** A rule may target a granular entity SUBTYPE (the `entity_type`
property — e.g. `theorem`, `definition`) via `EndpointKind::EntityOfType { kind,
entity_type }`. The subtype rule MUST bind its base entity kind: the matcher accepts an
endpoint only when the row's base kind equals `kind` AND its `entity_type` equals
`entity_type`. Do not reach for `EntityOfKind("theorem")` to target a subtype — that
variant compares the base kind alone (`concept`), so it is silently inert against a
subtype. Subtype matching relies on the `(EntityKind, entity_type)` registry invariant
(ADR-001); binding the base kind is what keeps a rule from matching a mistyped row.

```rust
// In khive-pack-formal: theorem -[depends_on]-> definition (both concept subtypes)
const EDGE_RULES: &[EdgeEndpointRule] = &[
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType { kind: "concept", entity_type: "theorem" },
        target: EndpointKind::EntityOfType { kind: "concept", entity_type: "definition" },
    },
];
```

`VerbRegistry::all_edge_rules()` aggregates contributions. The runtime's edge endpoint
validator (per ADR-002 + this aggregation) checks both the base contract and the pack
rules; an edge is legal if either accepts the triple.

### Storage profile and pack-auxiliary schema

Per ADR-003, each pack declares a `StorageProfile` describing its placement needs
(hot/cold/archive/read-only). The runtime assigns the pack to a backend based on
`khive.toml` configuration + the pack's profile.

Per ADR-015, packs also declare a `SchemaPlan` for pack-auxiliary tables (e.g., GTD's
lifecycle audit table, memory's index table). Pack schemas use `CREATE TABLE IF NOT
EXISTS` only and are applied at boot to the pack's assigned backend.

```rust
pub struct SchemaPlan {
    pub pack: &'static str,
    pub statements: &'static [&'static str],
}
```

Core substrate tables (entities, notes, edges, events) are NOT pack-owned — they are
owned by `khive-db` and evolve through versioned migrations (ADR-015). Pack schema is
strictly for pack-auxiliary tables.

### Dispatch path

```text
MCP request (or other transport, ADR-016)
  ↓
khive-request::parse_request(input) → ParsedRequest { ops, mode }
  ↓
For each ParsedOp:
  ↓
  VerbRegistry::find_verb_owner(op.tool) → Arc<dyn PackRuntime>
  ↓
  Gate::check(actor, namespace, op.tool, op.args) → GateDecision (ADR-018)
  ↓
  If Deny → RuntimeError::PermissionDenied
  If Allow → proceed
  ↓
  PackRuntime::dispatch(op.tool, op.args, &registry) → Result<Value, RuntimeError>
  ↓
  For shared CRUD verbs (create/list/update/etc): consult find_kind_hook for per-kind
  specialization
  ↓
  Pack handler calls into khive-runtime operations (ADR-014 curation, ADR-012
  retrieval, etc.)
  ↓
  Runtime calls storage traits (ADR-005)
  ↓
Response envelope (per ADR-016)
```

The registry sits between transport parsing and pack handlers. Every verb invocation
goes through it. This is the single dispatch site where gate enforcement, audit,
namespace propagation, and verb routing all converge.

### Built-in packs

The OSS distribution ships three packs. Each demonstrates a different use case for the
pack system:

| Pack       | Kinds                         | Verbs                                                                                                                                     | Edge rules                | Purpose                                                                                             |
| ---------- | ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------- | --------------------------------------------------------------------------------------------------- |
| **kg**     | 5 note kinds + 8 entity kinds | `create`, `list`, `get`, `update`, `delete`, `merge`, `search`, `link`, `neighbors`, `traverse`, `query`, `propose`, `review`, `withdraw` | (base ADR-002 only)       | Canonical research KG vocabulary + shared CRUD + event-sourced proposals (ADR-046)                  |
| **gtd**    | `task`                        | `assign`, `next`, `complete`, `tasks`, `transition`                                                                                       | `depends_on: task → task` | GTD task lifecycle (ADR-019)                                                                        |
| **memory** | `memory`                      | `remember`, `recall`                                                                                                                      | (none)                    | Decay-weighted memory (ADR-021); `delete(kind="memory")` covers removal — no separate `forget` verb |

External packs are sibling crates implementing `Pack + PackRuntime`. They register
through the same `VerbRegistryBuilder` mechanism as built-ins. There is no special-case
treatment for built-in packs.

### Pack registration

Packs are registered into the registry at runtime startup. The binary (kkernel) reads
the pack list from `RuntimeConfig.packs` (env var `KHIVE_PACKS` or `--pack` CLI flag)
and instantiates each pack:

```rust
let mut builder = VerbRegistry::builder()
    .with_default_namespace(config.default_namespace)
    .with_gate(config.gate)
    .with_event_store(runtime.events());

for pack_name in config.packs {
    let pack: Arc<dyn PackRuntime> = match pack_name.as_str() {
        "kg" => Arc::new(KgPack::new(runtime.clone())),
        "gtd" => Arc::new(GtdPack::new(runtime.clone())),
        "memory" => Arc::new(MemoryPack::new(runtime.clone())),
        other => return Err(BootError::UnknownPack(other.to_string())),
    };
    builder = builder.register(pack)?;
}

let registry = builder.build()?;
```

Unknown pack names are boot-time errors. Adding a pack requires:

1. Implementing `Pack + PackRuntime` in the pack's crate.
2. Adding the crate as a dependency to the binary (kkernel).
3. Adding a match arm in the binary's pack registration logic.

Steps 2 and 3 are intentional — they make pack adoption a deliberate operator choice,
not silent runtime loading. Dynamic plugin loading (dlopen-style) is deferred; v1 is
compile-time composition.

### Compile-time vs runtime composition

The pack system is **compile-time composition**, not runtime plugin loading. Packs are
linked into the binary at build time. The runtime configuration selects which compiled
packs to activate.

This is a deliberate choice. Dynamic loading would add:

- ABI compatibility burden (every pack must match the runtime's ABI).
- Version-skew risk (pack and runtime drift between versions).
- Security surface (loading arbitrary code from the filesystem).
- Linking complexity (`dlopen`, symbol resolution, lifetime management).

For OSS use cases (research KG, agent memory, task tracking), compile-time composition
is sufficient and significantly simpler. The pack crate ships its source; the operator
or distribution maintainer chooses which packs to include. If dynamic loading becomes
a real need (e.g., a marketplace of third-party packs), it gets its own ADR.

## Rationale

### Why const associated items in `Pack`?

Vocabulary is static — it doesn't depend on runtime state. Methods would force vtable
dispatch for every metadata lookup and require allocation for `Vec<&str>` returns.
Const items are zero-cost and `no_std`-compatible.

The cost is that `Pack` cannot be `dyn`. This is why `PackRuntime` exists as a mirror
with object-safe methods. The two-trait split is the standard Rust pattern for "static
declaration + dynamic dispatch."

### Why kg owns shared CRUD?

Putting shared CRUD (`create`, `list`, `update`, `delete`, etc.) directly in the
runtime would force the runtime to know about JSON shapes, discriminator validation,
and kind-specific hooks — all of which are MCP-shaped concerns. Keeping CRUD in the
kg pack keeps the runtime transport-agnostic: storage, query, hooks live in runtime;
the JSON dance lives in a pack.

The "kg pack" name is slightly misleading now (it carries CRUD for all kinds, not just
kg's own). Renaming would churn references without clarifying anything.

### Why KindHook for specialization?

Without hooks, every pack that adds a kind must also reimplement CRUD. The GTD pack
adding `task` would need parallel `create`/`list`/etc. handlers that mostly duplicate
kg's logic. That's bad: each pack carries ~500 LOC of CRUD boilerplate.

With hooks, kg's shared CRUD handles every kind. Hooks specialize where needed:
GTD's `TaskHook` normalizes task input shape and fires `depends_on` edges on creation.
Storage-shape kinds (a hypothetical `paper` kind with no lifecycle) need no hook —
they ride pure CRUD with zero pack code beyond `NOTE_KINDS` registration.

### Why closed relations + open endpoints?

The 15 edge relations are universal vocabulary — `depends_on` means the same thing in
every pack. Opening the relation set would lead to fragmentation: a `blocked_by`
synonym in pack A, a `precedes` synonym in pack B, no traversal can rationalize the
mess.

Endpoints are pack-specific: only GTD knows what a task is. Per-relation endpoint rules
let each pack declare which kinds it permits as endpoints, while the relation
vocabulary stays universal.

### Why additive-only edge rules?

Allowing packs to remove base-contract pairs would create order-dependent surfaces:
loading pack A might invalidate a query that worked under pack B alone. Strict
additivity preserves composability: every base-contract triple stays legal; packs only
broaden, never tighten.

### Why boot-time collisions, not runtime?

A verb collision discovered at runtime means a request fails when a previously-working
verb stops resolving. A boot-time collision means the operator sees the conflict
immediately, before anything depends on the wrong pack winning. Failing fast at boot
gives the operator a deterministic, debuggable startup; runtime collision detection
gives mysterious request failures.

### Why no special treatment for built-in packs?

If kg is a pack like any other, the system has one composition mechanism. If kg is
privileged ("the foundational pack"), the system has one mechanism for kg plus a
parallel mechanism for everything else. One mechanism is simpler to reason about,
test, and extend.

A future deployment that wants a different research vocabulary can replace kg with a
custom pack. The system supports it because there's no kg-specific machinery.

### Why compile-time over dynamic?

Dynamic loading is a meaningful complexity increase (ABI, versioning, security,
linking) for a benefit that doesn't materialize in OSS use cases. khive deployments are
binaries built from source by the operator or distribution. The set of packs is known
at build time.

If a marketplace of third-party dynamically-loaded packs becomes a real need, it gets a
dedicated ADR. v1 is compile-time.

## Alternatives Considered

| Alternative                                                            | Why rejected                                                                     |
| ---------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Feature-gated enums (kinds are compile-time enums with cargo features) | Requires rebuild per pack set; no runtime composition.                           |
| No validation (any string accepted as a kind)                          | Re-introduces vocabulary drift that closed taxonomies prevent.                   |
| Trait objects for `Pack` metadata                                      | Heap allocation; not `no_std`-compatible in `khive-types`.                       |
| Dynamic plugin loading (dlopen)                                        | ABI burden, version skew, security surface, linking complexity.                  |
| Single shared kind enum across all packs                               | Pollutes the kg taxonomy; couples pack additions to foundational crate releases. |
| Per-pack CRUD (every pack reimplements create/list/etc.)               | ~500 LOC of duplication per pack; cross-pack composition impossible.             |
| Open `EdgeRelation` enum (packs add relations)                         | Vocabulary fragmentation; traversal becomes pack-aware.                          |
| Subtractive edge rules (packs can remove base pairs)                   | Order-dependent surface; cross-pack incoherence.                                 |
| Runtime-only collision detection (no boot check)                       | Mysterious request failures instead of clear boot errors.                        |
| Special treatment for kg pack                                          | Two composition mechanisms; harder to reason about.                              |
| `Pack` trait in `khive-runtime` (not types)                            | Couples consumers of the trait to the full runtime stack.                        |
| One mega-verb per pack (`gtd(action="assign", args={...})`)            | Loses per-verb schema; redundant with the request DSL.                           |

## Consequences

### Positive

- One composition mechanism covers all extensions: vocabulary, verbs, hooks, edge
  endpoints, storage, schema.
- `khive-types` stays foundational and `no_std`-compatible. Pack declarations are
  const data.
- Built-in packs are not privileged. The system has one machinery, exercised by kg
  like by any third-party pack.
- Shared CRUD eliminates per-pack boilerplate. Storage-shape kinds need no pack code
  beyond kind registration.
- Closed relations + open endpoints preserves vocabulary stability while permitting
  pack-specific endpoint semantics.
- Boot-time collisions surface conflicts immediately, before any traffic.
- Audit-trail single chokepoint (registry dispatch site) makes gate enforcement and
  audit emission trivial to wire.

### Negative

- Two traits (`Pack` + `PackRuntime`) for one pack. The split is mechanical (const
  items can't live in `dyn` traits) but adds boilerplate to pack authors.
  Mitigated: a derive macro could auto-generate `PackRuntime` from `Pack`; deferred
  until pack count justifies it.
- `KindHook` operates on `serde_json::Value`, not a typed struct. Type errors surface
  at runtime, not compile time.
  Mitigated: pack-level integration tests; the JSON boundary is where transport meets
  runtime anyway.
- Compile-time composition means adding a pack requires rebuilding the binary.
  Mitigated: this is OSS; rebuilding is normal. A future dynamic-loading ADR can
  extend the model.
- Pack authors must reason about endpoint semantics — no compile-time check that a
  rule is meaningful.
  Mitigated: dead rules (referencing unregistered kinds) are boot-time errors; the
  pack author owns semantic correctness.

### Neutral

- The kg pack's name carries weight beyond its origins (it owns shared CRUD for all
  kinds). Renaming is not justified.
- `KindHook` covers `prepare_create` + `after_create`. Update and delete hooks extend
  the pattern when a real consumer asks.
- Pack auxiliary schema is non-evolving in v1. If a pack needs to evolve its schema, it
  coordinates with khive-db migrations (ADR-015).

## Implementation

- `crates/khive-types/src/pack.rs`:
  - `Pack` trait with const associated items.
  - `HandlerDef`, `Visibility`, `EdgeEndpointRule`, `EndpointKind` types.
- `crates/khive-runtime/src/pack.rs`:
  - `PackRuntime` trait (async, object-safe).
  - `KindHook` trait.
  - `VerbRegistry` + `VerbRegistryBuilder`.
  - Vocabulary merging, verb routing, hook dispatch.
  - Boot-time collision checks.
- `crates/khive-runtime/src/runtime.rs`:
  - `KhiveRuntime::install_edge_rules` (called by registry construction).
  - Pack-aware edge endpoint validation (consulting base + pack rules).
- `crates/kkernel/src/main.rs` (or wherever pack registration lives):
  - Pack instantiation from `RuntimeConfig.packs`.
- `crates/khive-pack-kg/`:
  - kg pack implementation (base vocabulary + shared CRUD verbs).
- `crates/khive-pack-gtd/`:
  - gtd pack implementation (task kind + lifecycle verbs + TaskHook).
- `crates/khive-pack-memory/`:
  - memory pack implementation (memory kind + recall verbs).

## References

- ADR-001: Entity Kind Taxonomy — kg pack registers the base entity kinds.
- ADR-002: Edge Ontology — closed relation enum; this ADR's `EDGE_RULES` extends
  the endpoint contract additively.
- ADR-003: System Architecture — kkernel composes packs at startup; SubstrateCoordinator
  routes cross-backend; `StorageProfile` declared by each pack.
- ADR-004: Substrate Observables — `NoteKindSpec` for pack-extensible kinds.
- ADR-013: Note Kind Taxonomy — kg pack registers the base 5 note kinds.
- ADR-014: Curation Operations — verbs that pack-owned and shared CRUD compose.
- ADR-015: Schema Migrations — pack `SchemaPlan` for pack-auxiliary tables; core
  substrate evolves via versioned migrations.
- ADR-016: Request DSL — the dispatch surface that routes through `VerbRegistry`.
- ADR-018: Authorization Gate — consulted at the same dispatch site as verb routing.
- ADR-019: GTD Pack — canonical example of a pack with lifecycle semantics.
- ADR-021: Memory Pack — canonical example of a decay-shape pack; uses `REQUIRES = ["kg"]`.
- ADR-023: Pack Verb Surface, Visibility, and Composition — third-party packs ship as
  Rust crates implementing the `Pack` trait and self-register via `inventory::submit!`;
  the YAML-manifest model is rescinded.
