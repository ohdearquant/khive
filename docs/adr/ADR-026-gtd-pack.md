# ADR-026: GTD Pack — Task Lifecycle over the Notes Substrate

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-025](ADR-025-pack-standard.md) introduced the `Pack` trait so vocabulary and verbs can be
extended without forking `khive-types`. The KG pack ships first; its §Context explicitly names
"a task-management plugin" as the canonical second pack. This ADR cashes that promise.

khive-internal has run a production task service for months — GTD-style status lifecycle, priority
tiers, assignee, dependencies — modelled as notes with `kind = "task"` plus structured
`properties`. The shape is well-validated and we want the OSS surface to match so anyone running
khive locally can run the same agent workflows.

Two design questions had to be settled:

1. **Substrate**: should tasks be a new substrate (table + store trait), or notes with
   `kind = "task"`?
2. **Verb names**: should the GTD pack reuse `create` / `list` / `update` / `delete` (collide with
   the KG pack) or introduce distinct verbs?

## Decision

### One pack crate, one note kind

Introduce `khive-pack-gtd` implementing `Pack + PackRuntime` with:

```rust
const NAME:          &str         = "gtd";
const NOTE_KINDS:    &[&str]      = &["task"];
const ENTITY_KINDS:  &[&str]      = &[];
const VERBS:         &[VerbDef]   = &["assign", "next", "complete", "tasks", "transition"];
```

Tasks ride on the existing `notes` table. The schema's `properties TEXT` column carries every GTD
field — `status`, `priority`, `assignee`, `due`, `start`, `end`, `depends_on`, `tags`,
`description`, `completed_at`, `transition_note`, `result`. No migration is needed; the pack's
only schema artefact is the additional `task` kind in the merged vocabulary set.

### GTD lifecycle (mirrors khive-internal)

```
inbox     → next | waiting | someday | active | done | cancelled
next      → active | waiting | someday | done | cancelled
active    → next | waiting | done | cancelled
waiting   → next | active | done | cancelled
someday   → next | active | done | cancelled
done      → next | active                   (reopen)
cancelled → next | active                   (reopen)
```

Aliases normalize at the boundary: `in_progress → active`, `todo → inbox`, `blocked → waiting`,
`later → someday`, `finished → done`. Priorities `p0`/`p1`/`p2`/`p3` map to salience
`1.0`/`0.75`/`0.5`/`0.25` so hybrid search can rank actionable items naturally. Same-status
transitions are idempotent no-ops.

### Five disjoint verbs (no `create` collision)

| Verb         | Purpose                                                                     |
| ------------ | --------------------------------------------------------------------------- |
| `assign`     | Create a task (note with `kind="task"`).                                    |
| `next`       | List actionable tasks (`status ∈ {next, active}`), priority-sorted.         |
| `complete`   | Validate transition to `done`, record `completed_at` and optional `result`. |
| `tasks`      | Filtered list — `status`, `assignee`, `priority`, pagination.               |
| `transition` | Explicit lifecycle change with `can_transition` validation.                 |

No collision with the KG pack's `create` / `list` / `update` / `delete`. ADR-025 §Verb routing
defers kind-discriminated routing; this pack avoids the deferral by choosing distinct verb names.

### Side-effects on `assign`

- `depends_on` IDs (resolved against the namespace, full UUID or 8-char short hex) are stored in
  `properties.depends_on` **and** recorded as `depends_on` graph edges (`EdgeRelation::DependsOn`).
  Edge creation is best-effort: a failed link is logged via `tracing::warn!` but does not abort the
  assign — the property captures the same information for queries.
- The body indexed for hybrid search is the task description (falling back to the title when no
  description is supplied), so `recall`-style queries against task content work the same as for
  any other note kind.

### Wire shape

Every successful op returns a stable task envelope:

```json
{
  "id":         "<8-char short>",
  "full_id":    "<rfc4122 uuid>",
  "kind":       "task",
  "title":      "...",
  "status":     "next",
  "priority":   "p1",
  "assignee":   "...",
  "due":        "2026-06-01T10:00:00Z",
  "namespace":  "local",
  "created_at": "<rfc3339>",
  "updated_at": "<rfc3339>",
  "properties": { ... full property bag ... }
}
```

Transitions return a delta-shaped envelope (`from`, `to`, `transitioned`, `is_terminal`,
optionally `note: "already in target status"`).

### Config-driven loading

Packs are registered into the [`VerbRegistry`] at startup based on
`RuntimeConfig::packs` — populated from the `KHIVE_PACKS` env var or the `--pack <name>` CLI flag.
Default remains `["kg"]` so existing single-pack consumers see no change. To run with GTD on a
fresh install:

```bash
KHIVE_PACKS=kg,gtd khive-mcp
# or:
khive-mcp --pack kg --pack gtd
```

The GTD-only plugin (`marketplace/gtd/`) ships a `plugin.json` whose `mcpServers.gtd.env.KHIVE_PACKS`
is `gtd`, so installing the plugin gives a task-only MCP surface without touching the binary.

## Rationale

### Why notes-as-tasks instead of a new substrate

- **Zero migration cost.** The notes table already has the columns we need; `properties JSON`
  carries GTD state without DDL changes.
- **Hybrid search comes for free.** Tasks ride the existing notes FTS5 + vector pipeline, so
  agents can recall "tasks about retrieval" the same way they recall observations.
- **Cross-pack composition.** A task note can be linked into the KG (e.g. `implements <concept>`)
  or annotated with insights via `annotates` edges (ADR-024) without bridging substrates.
- **Matches production.** khive-internal already proved this shape over the past three months.

### Why five verbs instead of reusing CRUD

- **Compile-time clarity.** ADR-025 §Verb routing notes that registered packs use first-wins
  collision routing. Distinct verbs avoid silently shadowing KG's `create` / `list`.
- **GTD-shaped APIs.** `assign` is the right verb for "make a task"; `next` matches the GTD
  vocabulary; `complete` enforces the legal-transition gate, which `update` would not.
- **Future kind-routing is unblocked.** When `kind`-discriminated routing eventually lands, this
  pack can opt in without rewriting consumer code.

### Why store `depends_on` in both properties and edges

- **Properties** keep the record self-describing — agents reading a task see its deps directly
  without an extra `neighbors` query.
- **Edges** make the dependency graph queryable: "what blocks task X?" is a one-hop traversal.
- The redundancy is bounded (only at write time, only on `assign`) and convergent (the property
  is authoritative; edge failures are non-fatal and logged).

### Why `transition` separately from `update`

`update` (KG pack) patches arbitrary properties. `transition` enforces the lifecycle table —
illegal jumps (`done → inbox`) are rejected with the allowed-set message. If a user wants to bypass
validation (e.g. data repair), they can still call `update` with `properties.status` set directly;
the KG pack is the escape hatch. `transition` is the agent-friendly path.

### Why `tasks` not `task_list`

Symmetric to KG's `list`. Underscore-separated tool names test poorly in the agent benchmarks we've
seen — they read like database-table names rather than verbs. `tasks` is a verb of inquiry
("show me tasks"), not a misspelled `list`.

## Alternatives Considered

| Alternative                                                            | Pros                                | Cons                                                                       | Why rejected                                          |
| ---------------------------------------------------------------------- | ----------------------------------- | -------------------------------------------------------------------------- | ----------------------------------------------------- |
| New `tasks` table + dedicated store trait + migration                  | Pure shape; indexed columns         | New substrate everywhere (migrations, traits, runtime ops, ~1000 LOC)      | The notes substrate already covers every query we run |
| Reuse `create` / `list` / `update` / `delete` with `kind="task"`       | Verb count stays small              | KG pack wins first-registered routing; would need kind-routing to fix      | ADR-025 explicitly defers kind routing                |
| One mega-verb `task(action="...", args={...})`                         | Single tool per pack                | Loses per-verb schema; redundant with the request DSL one level up         | The DSL already gives us batch composition            |
| Bundle GTD into KG pack                                                | One install                         | Couples research/KG workflows to task management; KG pack grows unbounded  | Each pack should own one coherent concern             |

## Consequences

### Positive

- The OSS surface now matches khive-internal's GTD verbs without forking the schema or substrate.
- Plugin authors get a clean composition story: the `gtd` plugin sets `KHIVE_PACKS=gtd` and ships
  only task-shaped tools.
- The deferred [ADR-025] §Verb-routing work doesn't gate this pack — distinct verb names avoid the
  collision entirely.
- Existing recall / search pipelines transparently surface tasks because they're just notes.

### Negative

- The `task` note kind is *not* a closed enum in code — it joins the open per-pack vocabulary set.
  Validators must consult `VerbRegistry::all_note_kinds()` rather than hard-coding the kg-list.
  Already the case post-ADR-025; this ADR reinforces the pattern.
- `next` / `tasks` scan up to 500 recent tasks and filter in-memory. Fine for personal/agent
  workloads (typical inboxes are < 50 actionable items); will need a property index or a v2 SQL
  path if anyone runs khive at hundreds-of-thousands of tasks scale.
- Same-status transitions are no-ops, which surprises callers who expected a write. The response
  body carries `transitioned: false` and an explanatory `note` field to make this auditable.

### Neutral

- The five verbs are stable; adding more (`defer`, `activate`, `archive`) is a forward-compatible
  vocabulary extension if real workflows demand them.

## Implementation Status

| Step                                                             | Where                                                     | Status |
| ---------------------------------------------------------------- | --------------------------------------------------------- | ------ |
| 1. New crate `khive-pack-gtd` with `Pack` + `PackRuntime` impl   | `crates/khive-pack-gtd/`                                  | done   |
| 2. GTD schema (statuses, priorities, lifecycle table)            | `crates/khive-pack-gtd/src/schema.rs`                     | done   |
| 3. Handlers — `assign`, `next`, `complete`, `tasks`, `transition`| `crates/khive-pack-gtd/src/handlers.rs`                   | done   |
| 4. Pack-config registration via `RuntimeConfig::packs`           | `crates/khive-runtime/src/runtime.rs`                     | done   |
| 5. MCP wiring (single `request` tool dispatches gtd verbs)       | `crates/khive-mcp/src/server.rs`                          | done   |
| 6. Tests (6 unit + 14 pack integration + 5 MCP-layer integration)| `crates/khive-pack-gtd/`, `crates/khive-mcp/tests/`       | done   |
| 7. Marketplace plugin entry with skills                          | `marketplace/gtd/`                                        | done   |

## References

- [ADR-019](ADR-019-note-kind-taxonomy.md): Note Kind Taxonomy (kg-owned closed set; gtd extends)
- [ADR-020](ADR-020-request-dsl.md): Request DSL (the transport for these verbs)
- [ADR-021](ADR-021-edge-relation-enum.md): EdgeRelation Enum (closed; `depends_on` is a member)
- [ADR-023](ADR-023-verb-consolidated-mcp-surface.md): KG verbs (sibling pack)
- [ADR-025](ADR-025-pack-standard.md): Pack Standard (composition mechanism this pack uses)
