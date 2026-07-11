# ADR-019: GTD Pack

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

The pack standard (ADR-017) specifies how vocabulary, verbs, kind specialization, and
edge endpoint rules compose into a runtime. khive ships three first-party packs as
canonical references for the system. The kg pack carries the foundational vocabulary
(ADR-001, ADR-013). The memory pack covers agent recall (**ADR-021**).

The GTD pack is the third — and the canonical example of a **lifecycle-shape pack**.
Where kg owns structural vocabulary (concepts, documents, observations) and memory
owns temporal recall, GTD owns a state-machine: tasks move through `inbox → next →
active → done | cancelled` over time, with explicit transitions and validation.

This ADR specifies the GTD pack: its kind, verbs, lifecycle, edge endpoints, kind
hook, and storage shape. It demonstrates that the pack standard supports non-KG
domains without forking the substrate.

The system must satisfy:

1. **No substrate fork.** Tasks ride on the existing notes table. No new storage
   trait, no migration, no parallel CRUD path. The `properties` JSON column carries
   GTD-specific fields.
2. **Five disjoint verbs.** No collision with the kg pack's shared CRUD. GTD's verbs
   express lifecycle intent (`assign`, `next`, `complete`, `tasks`, `transition`),
   not generic CRUD.
3. **Hybrid search reuse.** Tasks ride the existing notes FTS5 + vector pipeline.
   `search(kind="note", query=...)` over task content works the same as for any
   other note kind.
4. **Cross-pack composition.** A task can link into the KG via `implements <concept>`
   or be annotated by an `insight` note. The pack-extensible edge endpoint mechanism
   (ADR-017's `EDGE_RULES`) makes `depends_on: task → task` legal without changes
   to ADR-002.
5. **Two equivalent paths to the same outcome.** `gtd.assign(title="x")` and
   `create(kind="note", note_kind="task", title="x")` produce the same task record.
   The pack-owned verb is a flavored convenience over the shared CRUD path.

## Decision

### Pack identity

```rust
// crates/khive-pack-gtd/src/lib.rs
pub struct GtdPack { ... }

impl Pack for GtdPack {
    const NAME:         &'static str             = "gtd";
    const NOTE_KINDS:   &'static [&'static str]  = &["task"];
    const ENTITY_KINDS: &'static [&'static str]  = &[];
    const HANDLERS:     &'static [HandlerDef]    = &[
        HandlerDef { name: "gtd.assign",     description: "Create a task with optional dependencies.",       visibility: Visibility::Verb },
        HandlerDef { name: "gtd.next",       description: "List actionable tasks (status next or active).", visibility: Visibility::Verb },
        HandlerDef { name: "gtd.complete",   description: "Mark a task done with optional result.",         visibility: Visibility::Verb },
        HandlerDef { name: "gtd.tasks",      description: "Filtered task list.",                            visibility: Visibility::Verb },
        HandlerDef { name: "gtd.transition", description: "Explicit GTD status transition.",                visibility: Visibility::Verb },
    ];
    // ADR-023 §4: pack-prefixed verb names — `gtd.assign`, `gtd.next`, etc.
    const EDGE_RULES:   &'static [EdgeEndpointRule] = &[
        EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source:   EndpointKind::NoteOfKind("task"),
            target:   EndpointKind::NoteOfKind("task"),
        },
    ];
}
```

### Notes-as-tasks: zero substrate fork

A `task` is a note. `kind = "task"` is registered with the runtime via `NOTE_KINDS`.
The `properties` JSON column carries every GTD field:

```json
{
  "status": "next",
  "priority": "p1",
  "assignee": "operator",
  "due": "2026-06-01T10:00:00Z",
  "start": null,
  "end": null,
  "depends_on": ["abc12345", "def67890"],
  "tags": ["retrieval", "v1"],
  "description": "...",
  "completed_at": null,
  "transition_note": null,
  "result": null
}
```

The notes table schema is unchanged. The pack's only schema artifact is the additional
`"task"` value in `note.kind`. No DDL, no migration.

### GTD lifecycle

```text
inbox     → next | waiting | someday | active | done | cancelled
next      → active | waiting | someday | done | cancelled
active    → next | waiting | done | cancelled
waiting   → next | active | done | cancelled
someday   → next | active | done | cancelled
done      → (terminal — no outgoing transitions)
cancelled → (terminal — no outgoing transitions)
```

`done` and `cancelled` are **permanently terminal**: `allowed_transitions` returns `&[]`
for both states, and tests assert no transitions are permitted out of them (decision
GTD-AUD-001 / issue #273). Use `gtd.assign` to create a new task when reopening
semantics are required.

Transition validation lives in the `transition` verb handler. Illegal jumps (e.g.,
`done → inbox`) return `RuntimeError::InvalidInput` with the allowed-set message.
Same-status transitions are idempotent no-ops; the response carries `transitioned:
false` with an explanatory `note` field.

### Status / priority aliases

Aliases normalize at the verb boundary:

| Wire input    | Canonical |
| ------------- | --------- |
| `in_progress` | `active`  |
| `todo`        | `inbox`   |
| `blocked`     | `waiting` |
| `later`       | `someday` |
| `finished`    | `done`    |

Priorities `p0`/`p1`/`p2`/`p3` map to note `salience` `1.0`/`0.75`/`0.5`/`0.25` so
hybrid search (ADR-012) can rank actionable items naturally without needing
task-specific knowledge in retrieval.

### Five disjoint verbs

| Verb             | Purpose                                                                                                                                       |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `gtd.assign`     | Create a task. Args: `title`, `priority?`, `status?`, `assignee?`, `due?`, `depends_on?`, `tags?`, `description?`. Returns the task envelope. |
| `gtd.next`       | List actionable tasks (`status ∈ {next, active}`), priority-sorted. Args: `limit?`, `assignee?`.                                              |
| `gtd.complete`   | Validate transition to `done`, record `completed_at` and optional `result`. Args: `id`, `result?`.                                            |
| `gtd.tasks`      | Filtered list. Args: `status?`, `assignee?`, `priority?`, `limit?`, `offset?`.                                                                |
| `gtd.transition` | Explicit lifecycle change with full transition validation. Args: `id`, `status`, `note?`.                                                     |

No collision with kg pack's shared CRUD. ADR-017's `VerbRegistry` registers all five
verbs as `gtd`-owned. The kg pack's `create(kind="note", note_kind="task", ...)`
path also produces tasks — see "Two equivalent paths" below.

### `depends_on` as both property and edge

When `assign` (or shared `create`) creates a task with `depends_on: ["abc12345",
"def67890"]`:

1. The IDs are resolved within the caller's namespace (full UUID or 8-char prefix).
2. The full UUIDs are stored in `properties.depends_on` for self-describing record
   shape.
3. **Plus**: one `depends_on` edge is created per dependency, with the new task as
   source and the dependency as target.

The edge creation is legal because GTD's `EDGE_RULES` declares `depends_on: task →
task`. Without that rule, ADR-002's base contract (entity → entity for `depends_on`)
would reject the link.

Edge creation runs from `TaskHook::after_create`. Per ADR-017, post-write hook
failures are logged via `tracing::warn!` and do not propagate to the caller — the
storage write already succeeded. The property captures the same information; a
missing edge is a degraded result, not a failure.

### `TaskHook`: kind specialization for shared CRUD

The kg pack's shared `create` handles `note_kind="task"` through GTD's `KindHook`:

```rust
// crates/khive-pack-gtd/src/hook.rs
pub struct TaskHook;

#[async_trait]
impl KindHook for TaskHook {
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        // Normalize GTD-flavored input (title, priority, status, assignee, ...)
        // into kg's CreateParams shape (name, content, salience, properties).
        // ...
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        // Resolve depends_on IDs and create depends_on edges (best-effort).
        // Log on failure; do not propagate.
        // ...
    }
}

impl PackRuntime for GtdPack {
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "task" => Some(self.task_hook.clone()),
            _ => None,
        }
    }
    // ...
}
```

The kg pack's `create` handler:

1. Validates `note_kind="task"` against the merged vocabulary set (succeeds because
   GTD registered `task`).
2. Calls `registry.find_kind_hook("task")` → returns `Some(TaskHook)`.
3. `TaskHook::prepare_create(args)` normalizes the input.
4. Storage write via `NoteStore::upsert_note`.
5. `TaskHook::after_create(id, args)` fires `depends_on` edges.

### Two equivalent paths to the same task

Both forms produce equivalent task records:

**GTD-native** (flavored):

```text
gtd.assign(title="Implement retrieval", priority="p1", depends_on=["abc12345"])
```

**Shared CRUD** (generic):

```text
create(kind="note", note_kind="task", title="Implement retrieval",
       properties={"priority": "p1", "depends_on": ["abc12345"]})
```

The shared CRUD path runs through `TaskHook`, which normalizes the args. Both arrive
at the same `NoteStore::upsert_note` + `GraphStore::upsert_edge` calls. Agents pick
whichever fits their context: `assign` reads naturally for "I'm creating a task";
`create(kind="note", note_kind="task")` reads naturally for "I'm batching mixed
substrate operations through the request DSL."

### Lifecycle verbs stay pack-owned

`complete` and `transition` enforce the GTD state machine. They are not equivalent to
kg `update` — `update` patches arbitrary fields without lifecycle awareness, while
`transition` validates against the allowed-set table. A `done → inbox` `update` would
silently succeed; `gtd.transition(id, "inbox")` from `done` returns `InvalidInput`.

`next` and `tasks` are GTD-specific list queries. `tasks` filters by `status`,
`assignee`, `priority`. `next` is a specialized form of `tasks` returning actionable
items priority-sorted.

These verbs do not have shared-CRUD equivalents. Lifecycle semantics belong in the
pack that defines them.

### Hybrid search composition

Tasks ride the existing notes hybrid retrieval pipeline (ADR-012). The body indexed
for FTS5 + vectors is `task.description` (or the title when no description is
supplied). `search(kind="note", note_kind="task", query=...)` returns task notes
ranked by hybrid similarity.

This means agents can ask "tasks about retrieval" with `search(kind="note",
note_kind="task", query="retrieval")` — no GTD-specific machinery in retrieval.

Cross-substrate composition works the same way:

- `insight` notes annotating tasks: `link(insight_id, task_id, annotates)`
- Tasks linked to KG concepts: `link(task_id, concept_id, implements)` (legal per
  ADR-002 if the concept is an entity; the inverse — concept implements task — is
  not standard)
- Task dependencies: `link(task_a, task_b, depends_on)` (legal per GTD's
  `EDGE_RULES`)

### Storage profile and schema

GTD's `StorageProfile`:

```rust
impl PackRuntime for GtdPack {
    fn storage_profile(&self) -> StorageProfile {
        StorageProfile {
            roles: vec![PlacementRole::Hot],
            default_backend: "main",
        }
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "gtd",
            statements: &[
                // Optional lifecycle audit table (pack-auxiliary, idempotent).
                "CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit (
                    note_id    TEXT NOT NULL,
                    from_state TEXT NOT NULL,
                    to_state   TEXT NOT NULL,
                    note       TEXT,
                    at         INTEGER NOT NULL,
                    namespace  TEXT
                )",
                "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT",
                "CREATE INDEX IF NOT EXISTS idx_gtd_audit_note
                    ON gtd_lifecycle_audit(note_id, at DESC)",
            ],
        }
    }
    // ...
}
```

`gtd_lifecycle_audit` is a pack-auxiliary table. It records each `transition`
and `complete` invocation for replay and compliance, including the caller namespace.
The `namespace` column is nullable because it was added after the table shipped:
legacy rows may be `NULL`, while new rows always bind the authorized namespace.
Per ADR-015, pack schema uses idempotent declarations by default; GTD's nullable
namespace `ALTER TABLE` is the documented v1 pack-local evolution exception.

`StorageProfile.roles: [Hot]` because task work is interactive — tasks are read and
updated constantly. `default_backend: "main"` keeps tasks on the same backend as kg
data, allowing tasks to link to concepts in the same SQLite file without cross-backend
coordination.

### Wire shape

Every successful op returns a stable task envelope:

```json
{
  "id":         "<8-char short>",
  "full_id":    "<rfc4122 uuid>",
  "kind":       "task",
  "title":      "Implement retrieval",
  "status":     "next",
  "priority":   "p1",
  "assignee": "operator",
  "due":        "2026-06-01T10:00:00Z",
  "namespace":  "local",
  "created_at": "2026-05-23T01:55:00Z",
  "updated_at": "2026-05-23T01:55:00Z",
  "properties": { ... full property bag ... }
}
```

`transition` returns a delta envelope:

```json
{
  "id": "<8-char short>",
  "from": "next",
  "to": "active",
  "transitioned": true,
  "is_terminal": false
}
```

Same-status transitions return `{"transitioned": false, "note": "already in target
status"}` so callers can audit the no-op.

### Config-driven loading

GTD is opt-in via `RuntimeConfig::packs`:

```bash
KHIVE_PACKS=kg,gtd kkernel mcp
# or
kkernel mcp --pack kg --pack gtd
```

Default (`KHIVE_PACKS` unset) is `["kg"]` only — personal-local users who do not need
task tracking see no GTD verbs in the catalog (ADR-016's dynamic verb catalog reflects
exactly what's loaded).

The GTD plugin (`marketplace/gtd/`) ships a `plugin.json` whose
`mcpServers.gtd.env.KHIVE_PACKS = "gtd"` for a task-only MCP surface (no kg verbs).
This lets agents that only need GTD have a minimal verb set without dragging in
research-KG concerns.

## Rationale

### Why notes-as-tasks instead of a new substrate?

- **Zero migration cost.** The notes table already has every column GTD needs;
  `properties JSON` carries the lifecycle fields without DDL changes.
- **Hybrid search comes for free.** Tasks ride the existing notes FTS5 + vector
  pipeline. Agents recall "tasks about X" the same way they recall observations.
- **Cross-pack composition.** A task note can be linked into the KG via standard
  edges (`implements <concept>`) or annotated by insight notes via `annotates` —
  without bridging substrates.
- **Matches production usage.** This shape has been validated over months of real
  agent workflows.

A new `tasks` substrate would mean a new store trait, new migrations, new validation
paths, new query mechanisms, ~1000 LOC of duplicated machinery for what `properties`
JSON already handles.

### Why five disjoint verbs instead of reusing CRUD?

- **No collision with kg.** ADR-017's `VerbRegistry` rejects duplicate verb names at
  boot. Choosing distinct names (`assign`, `next`, etc.) avoids that constraint
  entirely.
- **GTD-shaped API.** `assign` is the right verb for "make a task." `next` matches
  GTD vocabulary. `complete` enforces the legal-transition gate — `update` would
  not.
- **Two paths, equivalent outcomes.** The shared CRUD path (`create(kind="note",
  note_kind="task", ...)`) produces the same task. Agents that prefer the
  flavored API use `assign`; agents that prefer batching mixed substrate ops
  through the request DSL use `create`. Both work.

### Why store `depends_on` in both properties and edges?

- **Properties keep the record self-describing.** An agent reading a task sees its
  dependencies directly without an extra `neighbors` query.
- **Edges make the dependency graph queryable.** "What blocks task X?" is a one-hop
  traversal via `neighbors(node_id=X, direction="in", relations=["depends_on"])`.
- The redundancy is bounded (only at write time, only on `assign`) and convergent
  (the property is authoritative; edge failures from `after_create` are non-fatal
  per ADR-017).

ADR-017's pack-extensible edge endpoints (`EDGE_RULES`) is what makes the edge legal.
Without that mechanism, ADR-002's base contract would reject `depends_on: task →
task`.

### Why `transition` separately from `update`?

`update` (kg pack) patches arbitrary properties. `transition` enforces the GTD
lifecycle table — illegal jumps (`done → inbox`) are rejected. The two verbs serve
different intents:

- `update(id, properties={"due": "2026-07-01"})` — patch a field, no lifecycle.
- `gtd.transition(id, status="active")` — change lifecycle state with validation.

A future agent might call `update(id, properties={"status": "done"})` and silently
bypass the lifecycle validation. The `transition` verb forces the explicit lifecycle
intent and validates accordingly.

### Why `tasks` not `task_list`?

Symmetric to kg's `list`, but flavored. Underscore-separated tool names read like
database tables; `tasks` reads like a verb of inquiry ("show me tasks"). Agent
benchmark testing shows verb-shaped names outperform table-shaped names for
LLM-generated tool calls.

### Why `next` as a specialized list?

`next` is the GTD-canonical query: "what should I work on right now?" It's a specific
filter (`status ∈ {next, active}`) with a specific sort (priority-descending). Could
be expressed as `gtd.tasks(status=["next", "active"], sort="priority desc")`, but `gtd.next()`
is one of the most-called verbs in practice and deserves its own name.

### Why pack-auxiliary `gtd_lifecycle_audit` table?

Lifecycle transitions are operationally significant. Compliance, retrospectives, and
debugging benefit from a queryable record of "task X went from state A to state B at
time T with note N in namespace NS." The audit table records each `transition` and
`complete` invocation. Legacy rows created before the namespace backfill may have
`NULL` namespace.

This is GTD-specific data; it doesn't belong in the core `events` table (which is
governed by ADR-004 and used for system-level events). Pack-auxiliary tables (per
ADR-015) are the right placement.

### Why opt-in loading?

Personal-local users who do not need task tracking shouldn't see five extra verbs in
the catalog (ADR-016 cost) or pay schema overhead. Default `["kg"]` keeps the surface
minimal. Operators who want GTD configure it explicitly.

## Alternatives Considered

| Alternative                                                                          | Why rejected                                                                                         |
| ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| New `tasks` table + dedicated store trait + migration                                | New substrate everywhere (~1000 LOC); notes substrate already covers every query.                    |
| Reuse `create` / `list` / `update` / `delete` with `kind="task"` only (no GTD verbs) | Loses lifecycle semantics; `update` doesn't validate transitions; no `assign` ergonomics.            |
| One mega-verb `task(action="...", args={...})`                                       | Loses per-verb schema; redundant with the request DSL one level up.                                  |
| Bundle GTD into kg pack                                                              | Couples research/KG workflows to task management; kg grows unbounded; violates one-pack-one-concern. |
| `depends_on` as property only (no edge)                                              | Loses graph traversal for blockers ("what blocks task X?" needs full scan).                          |
| `depends_on` as edge only (no property)                                              | Records are not self-describing; every read needs a `neighbors` query for deps.                      |
| Lifecycle validation in shared CRUD                                                  | Forces kg to know about GTD's state machine; couples packs.                                          |
| Always load GTD by default                                                           | Surface bloat for users who don't need tasks; conflicts with the opt-in pack model.                  |
| Audit transitions to the core `events` table                                         | Pack-specific audit data; pollutes core events; pack-auxiliary table is right.                       |

## Consequences

### Positive

- The verb surface gains a production-quality GTD verb set without forking the schema
  or substrate.
- The pack standard (ADR-017) is validated against a non-KG use case — the system
  proves itself extensible.
- Plugin authors get a clean composition story: ship a pack crate, register kinds and
  verbs, optionally specialize via `KindHook`.
- Existing recall/search pipelines transparently surface tasks because they're notes.
- Two paths to the same outcome (`assign` vs `create(kind="note", note_kind="task")`)
  serve different agent contexts without duplicating logic.
- `EDGE_RULES` mechanism is exercised — task dependencies are graph-traversable, not
  just property-encoded.

### Negative

- The `task` note kind is not a compile-time enum value. Validators must consult
  `VerbRegistry::all_note_kinds()`, not hard-coded sets.
  Mitigated: ADR-017's pattern; new packs work the same way.
- `next` and `tasks` both push status/assignee/priority predicates into SQL
  (issue #772) instead of pre-fetching a fixed unfiltered recency window, but
  they bound the candidate set differently:
  - `next` must priority-sort the _entire_ actionable (`next`/`active`) set
    before applying the caller's `limit`, so it fetches every matching row
    in one deterministically-ordered snapshot query
    (`query_notes_filtered_bounded`, capped at 20,001 rows) rather than a
    page loop — a page loop's separate `COUNT(*)` and per-page reads have no
    spanning transaction, so a concurrent insert between pages can duplicate
    or skip a boundary row. If more than 20,000 rows match, `next` returns an
    explicit `InvalidInput` error asking the caller to narrow the filters
    (e.g. add `assignee`) rather than ever sorting and returning a possibly
    priority-incomplete set.
  - `tasks` has no such bound: it is a caller-paginated `list`-style verb —
    the caller supplies `limit`/`offset` via `query_notes_filtered` and pages
    through results themselves, the same as any other paginated query verb.
    There is no server-side row cap or over-bound rejection for `tasks`.

  Fine for personal/agent workloads (typical inboxes < 50 actionable items);
  `next`'s full-scan-then-sort will need property indexes or a v2 SQL path
  (e.g. `ORDER BY` pushed into SQL) at hundreds-of-thousands scale.
- Same-status transitions are no-ops, which can surprise callers expecting a write.
  Mitigated: `transitioned: false` + `note` field in the response body.
- `depends_on` redundancy (property + edge) requires both writes to succeed for
  consistency. Edge write is best-effort; on failure, the property still holds.
  Mitigated: documented; future relibilization (atomic two-write transaction) is a
  v2 enhancement.

### Neutral

- The five verbs are stable. Adding more (`defer`, `activate`, `archive`) is a
  forward-compatible vocabulary extension.
- `gtd_lifecycle_audit` is pack-auxiliary; its presence is invisible to non-GTD
  packs.
- Task properties (priority, status, assignee, etc.) remain free-form JSON — no
  schema enforcement beyond what `transition` validates.

## Implementation

- `crates/khive-pack-gtd/src/lib.rs`:
  - `GtdPack` struct + `Pack` impl (consts) + `PackRuntime` impl (dispatch).
  - `kind_hook("task")` returns `Some(TaskHook)`.
- `crates/khive-pack-gtd/src/handlers.rs`:
  - `assign`, `next`, `complete`, `tasks`, `transition` handlers.
  - Lifecycle state machine (`can_transition`, allowed-set messages).
  - Status / priority aliasing.
- `crates/khive-pack-gtd/src/hook.rs`:
  - `TaskHook` implementing `KindHook`.
  - `prepare_create`: normalize GTD args into kg shape.
  - `after_create`: fire `depends_on` edges (best-effort, logged on failure).
- `crates/khive-pack-gtd/src/schema.rs`:
  - `gtd_lifecycle_audit` table DDL.
- `crates/kkernel/src/server.rs` (or pack registration):
  - Conditional `GtdPack` registration based on `RuntimeConfig::packs`.
- `marketplace/gtd/plugin.json`:
  - Plugin manifest with `KHIVE_PACKS=gtd` env setting for task-only deployments.

## References

- ADR-002: Edge Ontology — `depends_on` relation and base endpoint contract.
- ADR-005: Storage Capability Traits — `NoteStore`, `GraphStore` used by GTD.
- ADR-012: Retrieval Composition — hybrid search composes tasks via the notes
  pipeline.
- ADR-013: Note Kind Taxonomy — base 5 kinds owned by kg; GTD adds `task`.
- ADR-014: Curation Operations — shared CRUD verbs (`create`, `update`, `delete`)
  handle task notes through `TaskHook`.
- ADR-015: Schema Migrations — pack-auxiliary tables use idempotent
  `CREATE TABLE IF NOT EXISTS` by default; GTD's nullable audit namespace
  backfill is the documented pack-local `ALTER TABLE` exception.
- ADR-016: Request DSL — verb dispatch surface that routes to GTD's verbs.
- ADR-017: Pack Standard — `Pack`, `PackRuntime`, `KindHook`, `VerbRegistry`,
  `EDGE_RULES` — the mechanism GTD demonstrates.
- ADR-018: Authorization Gate — gate enforcement applies to GTD verbs like any
  other.
