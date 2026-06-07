# khive-pack-gtd Design

## ADR Compliance

### ADR-002: Edge Ontology (15 edge relations — closed set)
- The GTD pack does NOT add new edge relation variants; `depends_on` is already in the base set.
- The pack additively extends the *endpoint contract* to allow `depends_on` between two `task`
  notes (base contract restricts it to entity→entity). This is pack-extensible per ADR-017 rules.
- Edge relation enum remains closed; packs may only extend endpoint pairs, not add new relations.

### ADR-004: NoteKindSpec lifecycle declaration
- `GtdPack` declares a `NoteKindSpec` for the `task` note kind.
- Lifecycle field is named `kind_status` (NOT `status`) to avoid semantic collision with
  `Note.status`, which is a row-visibility field always set to `"active"` for live rows.
- GTD lifecycle status lives in `properties["status"]` at storage layer (Phase 1); migration to a
  first-class `kind_status` column is planned for Phase 2 (c11/c12 milestones).
- Terminal states: `done`, `cancelled`. No outgoing transitions are permitted from terminal states.
  This is intentional and differs from the original ADR-019 draft which considered reopen semantics.
  The no-reopen rule is authoritative; use `gtd.assign` to create a new task instead.

### ADR-017: Pack Standard (Pack trait, `EDGE_RULES`, pack-extensible edge endpoints)
- `GtdPack` implements the `Pack` trait and `PackRuntime` trait.
- Declares vocabulary via constants: `NOTE_KINDS`, `ENTITY_KINDS`, `HANDLERS`, `EDGE_RULES`,
  `NOTE_KIND_SPECS`, `SCHEMA_PLAN`.
- The `TaskHook` implements the `KindHook` extension point: normalizes GTD fields on
  `prepare_create` and wires `depends_on` graph edges on `after_create` (best-effort).
- `EDGE_RULES` contains one rule: `depends_on` between two `task` notes (task→task).
- Endpoint rules are additive only — this pack cannot tighten the base contract.

### ADR-019: GTD lifecycle contract
- Five verbs: `gtd.assign`, `gtd.next`, `gtd.complete`, `gtd.tasks`, `gtd.transition`.
- Lifecycle states: `inbox → next | waiting | someday | active | done | cancelled`.
- `done` and `cancelled` are permanently terminal (no reopen; issue #273).
- `complete()` is restricted to actionable states (`next`, `active`). Tasks in `inbox`,
  `waiting`, or `someday` must be explicitly transitioned to an actionable state first.
- `gtd_lifecycle_audit` table records every `transition` and `complete` invocation for
  replay and compliance. Writes are best-effort (non-fatal on failure).
- `depends_on` property stores UUIDs of blocking tasks; `gtd.next` excludes tasks whose
  blockers are not in `done` state (scenario-gtd C2).

### ADR-025: Illocutionary verb classification (Searle 1976)
- `gtd.assign` → Directive (directs an actor to perform work)
- `gtd.next` → Assertive (retrieves actionable task state)
- `gtd.tasks` → Assertive (retrieves filtered task listing)
- `gtd.complete` → Declaration (changes task institutional status to terminal)
- `gtd.transition` → Declaration (changes task lifecycle status by fiat)

### ADR-027: Inventory self-registration
- `GtdPack` self-registers via `inventory::submit!` so it can be loaded dynamically from
  the pack registry by name (`"gtd"`) without a hard compile-time dependency in the MCP binary.
- Requires `"kg"` pack as a dependency (`REQUIRES = &["kg"]`).

### ADR-030: Non-propagating after_create failures
- If `depends_on` edge creation fails after the task note is successfully written,
  the error is logged and swallowed. A `properties["depends_on"]` key captures the same
  dependency information for queries that bypass the graph layer.
- This avoids misleading the caller with `ok: false` for a task that is already on disk.

### ADR-031: GTD pack-extensible edge rule for task blockers
- The GTD pack's `EDGE_RULES` extends the base `depends_on` endpoint contract to allow
  task-note→task-note links.
- Pre-validation in `gtd.assign` and `TaskHook.prepare_create` ensures the target of each
  `depends_on` UUID is a `task` note before any storage write. This preserves the
  atomicity invariant: no task is persisted if its dependency chain is invalid.

## Consistency Notes

### Terminal-state behavior vs ADR-019 draft
- ADR-019 was originally drafted with reopen semantics in mind. The implementation
  explicitly closes terminal states (`done`, `cancelled`) — no outgoing transitions.
  ADR-019 should be amended to reflect the no-reopen rule as the authoritative contract.

### GTD status vs row-visibility status (W1-G remap)
- `Note.status` is a row-visibility field (`"active"` for live rows, never the GTD state).
- GTD lifecycle status lives in `properties["status"]`.
- The KG `get` and `list` handlers apply a remap: `properties.status` is promoted to the
  top-level `status` field; the row-visibility value moves to `lifecycle`. Tests verify this.

### `complete()` actionable-state gate (UE2-H1)
- `complete()` rejects tasks in non-actionable states (`inbox`, `waiting`, `someday`) with
  a message directing the caller to transition first. `transition(status=done)` bypasses
  this gate for use cases where direct terminal transition is intended.

### Atomic transition (ue-dsl-parallel C2)
- Both `complete()` and `transition()` use a conditional SQL UPDATE with a
  `json_extract(properties, '$.status') = expected_current` WHERE predicate. This ensures
  that concurrent calls in a parallel DSL batch only one wins; the other gets
  `rows_affected = 0` and returns an error rather than a false success.
