# khive-pack-gtd Design

## ADR Compliance

### Edge Ontology (17 edge relations — closed set; 15 base + 2 epistemic via ADR-055) (ADR-002)

- The GTD pack does NOT add new edge relation variants; `depends_on` is already in the base set.
- The pack additively extends the _endpoint contract_ to allow `depends_on` between two `task`
  notes (base contract restricts it to entity→entity). This is pack-extensible per ADR-017 rules.
- Edge relation enum remains closed; packs may only extend endpoint pairs, not add new relations.

### NoteKindSpec lifecycle declaration (ADR-004)

- `GtdPack` declares a `NoteKindSpec` for the `task` note kind.
- Lifecycle field is named `kind_status` (NOT `status`) to avoid semantic collision with
  `Note.status`, which is a row-visibility field always set to `"active"` for live rows.
- GTD lifecycle status lives in `properties["status"]` at storage layer (Phase 1); migration to a
  first-class `kind_status` column is planned for Phase 2 (c11/c12 milestones).
- Terminal states: `done`, `cancelled`. No outgoing transitions are permitted from terminal states.
  This is intentional and differs from the original ADR-019 draft which considered reopen semantics.
  The no-reopen rule is authoritative; use `gtd.assign` to create a new task instead.

### Pack Standard (Pack trait, `EDGE_RULES`, pack-extensible edge endpoints) (ADR-017)

- `GtdPack` implements the `Pack` trait and `PackRuntime` trait.
- Declares vocabulary via constants: `NOTE_KINDS`, `ENTITY_KINDS`, `HANDLERS`, `EDGE_RULES`,
  `NOTE_KIND_SPECS`, `SCHEMA_PLAN`.
- The `TaskHook` implements the `KindHook` extension point: normalizes GTD fields on
  `prepare_create`, synchronizes task content/description on `prepare_note_update`,
  and wires `depends_on` graph edges on `after_create` (best-effort).
- Generic create validates the raw shared `CreateParams` shape before `prepare_create`,
  so normalization cannot hide malformed `name`, `content`, or `salience` values.
- Update normalization and persistence share one note snapshot. Canonical persistence
  compare-and-swaps it; atomic persistence carries the same revision guard into commit.
- `EDGE_RULES` contains one rule: `depends_on` between two `task` notes (task→task).
- Endpoint rules are additive only — this pack cannot tighten the base contract.

### GTD lifecycle contract (ADR-019)

- Five verbs: `gtd.assign`, `gtd.next`, `gtd.complete`, `gtd.tasks`, `gtd.transition`.
- Lifecycle states: `inbox → next | waiting | someday | active | done | cancelled`.
- `done` and `cancelled` are permanently terminal (no reopen; issue #273).
- `complete()` validates its `done`/`cancelled` target against the same lifecycle table as
  `transition`; every non-terminal state has a legal direct terminal transition.
- `gtd_lifecycle_audit` receives best-effort rows for successful real transitions,
  completions, and canonical same-status note events. Writes are non-fatal on failure,
  and attempted-write responses expose `audit_persisted` so loss is never silent.
- `depends_on` property stores UUIDs of blocking tasks; `gtd.next` excludes tasks whose
  blockers are not in `done` state by default. Query results report `dependency_state`,
  `actionable`, and structural `blocked_by` diagnostics; `include_blocked=true` makes
  `gtd.next` include blocked or broken candidates after ready work.

### Illocutionary verb classification (Searle 1976) (ADR-025)

- `gtd.assign` → Directive (directs an actor to perform work)
- `gtd.next` → Assertive (retrieves actionable task state)
- `gtd.tasks` → Assertive (retrieves filtered task listing)
- `gtd.complete` → Declaration (changes task institutional status to terminal)
- `gtd.transition` → Declaration (changes task lifecycle status by fiat)

### Inventory self-registration (ADR-027)

- `GtdPack` self-registers via `inventory::submit!` so it can be loaded dynamically from
  the pack registry by name (`"gtd"`) without a hard compile-time dependency in the MCP binary.
- Requires `"kg"` pack as a dependency (`REQUIRES = &["kg"]`).

### Non-propagating after_create failures (ADR-019)

- If `depends_on` edge creation fails after the task note is successfully written,
  the error is logged and swallowed. A `properties["depends_on"]` key captures the same
  dependency information for queries that bypass the graph layer.
- This avoids misleading the caller with `ok: false` for a task that is already on disk.

### Pack-extensible edge rule for task blockers (ADR-017)

- The GTD pack's `EDGE_RULES` extends the base `depends_on` endpoint contract to allow
  task-note→task-note links (ADR-017 Pack Standard §"Pack-extensible edge endpoints").
- Pre-validation in `gtd.assign` and `TaskHook.prepare_create` ensures the target of each
  `depends_on` UUID is a `task` note before any storage write. This preserves the
  atomicity invariant: no task is persisted if its dependency chain is invalid.
- The task hook also validates generic KG property updates and task-to-task `depends_on`
  links, rejecting direct, transitive, and same-batch dependency cycles before writes.

## Consistency Notes

### Terminal-state behavior — ADR-019 authoritative contract

- `done` and `cancelled` are permanently terminal; no outgoing transitions are permitted.
  ADR-019 has been amended to reflect this as the authoritative contract.
  Use `gtd.assign` to create a new task when reopening semantics are required.

### GTD status vs row-visibility status (W1-G remap)

- `Note.status` is a row-visibility field (`"active"` for live rows, never the GTD state).
- GTD lifecycle status lives in `properties["status"]`.
- The KG `get` and `list` handlers apply a remap: `properties.status` is promoted to the
  top-level `status` field; the row-visibility value moves to `lifecycle`. Tests verify this.

### `complete()` lifecycle-table parity

- `complete()` uses the same `can_transition` contract as `transition`: `inbox`, `next`,
  `active`, `waiting`, and `someday` may all move directly to `done` or `cancelled`.
  `done` and `cancelled` remain permanently terminal.

### Atomic transition

- Both `complete()` and `transition()` use a conditional SQL UPDATE with a
  WHERE predicate over the exact decision snapshot's `updated_at`, `deleted_at`, and
  semantic GTD status. Missing legacy `properties.status` is compared as `inbox`.
  This ensures that concurrent lifecycle calls and generic task updates cannot
  overwrite one another: the loser gets `rows_affected = 0` and returns an error,
  while a mixed atomic unit rolls back in full.
