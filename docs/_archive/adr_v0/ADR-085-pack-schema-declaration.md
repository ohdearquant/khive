# ADR-085: Pack Schema Declaration and Application Order

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-022 (Schema Migrations), ADR-025 (Pack Standard), ADR-079 (Pack-Scoped
Backends — declares which backend hosts each pack)\
**Extends**: ADR-025 §"PackRuntime" — adds `schema_plan()` to the Pack trait

## Context

ADR-079 introduces pack-scoped backends — each pack is assigned to exactly one declared
backend in `khive.toml`. Once packs own their backend assignment, the next question is who
owns the **schema** on that backend: which tables, indexes, and migrations does each pack
need, and how are they applied at boot?

In khive-internal, this was handled per-service (e.g., `khive_lore::service::SCHEMA_PLAN`
applied to the lore backend). The open-core port currently has shared substrate tables
(`entities`, `notes`, `events`, `graph_edges`) defined in `khive-db` migrations, with no
mechanism for packs to declare additional pack-specific tables.

This ADR adds that mechanism — the `Pack` trait gains a schema plan declaration, and the
boot process applies each pack's plan to its assigned backend in TOML declaration order.

## Decision

### Pack trait gains `schema_plan()`

```rust
pub trait Pack: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;

    /// Schema plan for this pack's substrate extensions.
    /// None = pure compute (no DB tables beyond shared substrate).
    fn schema_plan(&self) -> Option<&ServiceSchemaPlan>;

    // ... existing trait methods unchanged
}
```

`ServiceSchemaPlan` already exists in `khive-db::migrations` per ADR-022. Each pack's plan is
applied to its assigned backend at boot, idempotently, with per-pack version tracking via the
existing `_schema_migrations` table (one row per pack-versioned migration).

The line between **substrate** (in `khive-db`) and **pack extension** (in the pack's
schema_plan) follows ADR-025's pack pattern:

- Anything inherent to a substrate kind (entity, note, edge, event) lives in `khive-db` base
  migrations.
- Anything pack-specific (memory salience columns, task lifecycle columns, brain posteriors)
  lives in the pack's schema_plan.

### Table naming convention to avoid collisions

When two packs share a backend (per ADR-079 — multiple packs may have `backend = "main"`),
both schema plans apply to the same SQLite file. Table-name collisions become possible.

**Convention** (binding for all packs):

- Pack-specific tables: prefixed with the pack name. e.g., `kg_entities` (kg pack's
  pack-specific entity extensions), `lore_atoms`, `memory_salience`.
- Shared substrate tables (`entities`, `notes`, `events`, `graph_edges`, `vec_*`, `fts_*`)
  live in `khive-db` base migrations and are owned by the substrate layer, not by packs.
- A pack only declares additional tables it needs beyond the substrate.

### D7 — Schema applied in TOML declaration order

For two packs sharing a backend, their schema plans apply in **TOML declaration order**.
Collisions on table names = boot failure with explicit error naming both packs and the
conflicting table.

```toml
# This order is also the schema-apply order on the shared backend.
[packs.kg]     = { backend = "main", ... }    # kg's plan applies first
[packs.gtd]    = { backend = "main", ... }    # then gtd's plan
[packs.memory] = { backend = "main", ... }    # then memory's plan
[packs.lore]   = { backend = "lore", ... }    # separate backend; independent order
```

**Why declaration order, not topological sort**:

- **Predictable.** Operator-controlled, no implicit ordering surprises.
- **Pack-author-readable.** A pack author who needs to run after another pack documents this in
  their pack README ("load me after pack X"); operators place the entries accordingly.
- **Cheap collision detection.** One `CREATE TABLE IF NOT EXISTS` per plan + a check for
  existing-table column mismatch is enough to prevent silent schema drift between packs.

If a future pack needs to extend another pack's tables (rather than declare its own), that
relationship requires its own ADR. Extension hooks vs. inheritance is a different question
than schema application order, and is out of scope here.

### Collision policy

When pack A and pack B both declare the same table name on the same backend, boot fails with:

```
SchemaCollision { backend: "main", table: "tasks", packs: ["gtd", "tasktracker"] }
```

The error names both packs and the conflicting table. Boot does not proceed; the operator must
either rename one pack's table (by editing the pack) or move one pack to a different backend.

Auto-prefixing (silently renaming `tasks` → `gtd_tasks`) is **rejected**. It would hide bugs
where two packs unintentionally claim the same logical table.

## Layering

This ADR is the pack-author contract. The reading audience is:

- **Operators** — when they share a backend across packs, they read this to understand the
  collision rule and table naming convention.
- **Pack authors** — when they implement a pack, they read this to know they must implement
  `schema_plan()` (or return `None` for pure-compute packs) and follow the naming convention.

| Concern                               | Crate                                           | Why                           |
| ------------------------------------- | ----------------------------------------------- | ----------------------------- |
| `Pack` trait + `schema_plan()` method | `khive-runtime` (pack trait already lives here) | Trait extension               |
| `ServiceSchemaPlan` type              | `khive-db` (already exists per ADR-022)         | Migration plan structure      |
| Per-pack schema declaration           | Each pack crate                                 | Pack-author owns their schema |
| Schema application at boot            | `kkernel` (ADR-076) or `khive-mcp` interim      | Boot orchestrates             |
| Collision detection                   | `kkernel` or `khive-mcp` interim                | Boot fails fast on collision  |

## Migration Plan

This ADR corresponds to **Phase B3** in
`.khive/notes/plans/plan_20260522_runtime_restoration.md` — turn on per-pack schema
declaration.

1. Add `schema_plan()` to the Pack trait (default impl returning `None`).
2. Implement `schema_plan()` for each existing pack (kg, gtd, memory, brain). For v1, most
   existing tables stay in `khive-db` as substrate; new tables introduced after this ADR go
   into pack-specific plans.
3. Boot path applies each pack's plan to its backend in declaration order.
4. Collision detection wired in boot: each `CREATE TABLE` is followed by a verification step
   against the previous pack's schema state.

Phase B3 ships independently after Phase B2 (TOML config + multi-backend wiring) lands.

## Alternatives Considered

### A. Topological sort by pack dependencies

`Pack::REQUIRES` (ADR-037) already declares pack dependencies for vocabulary; reuse the same
dependency graph for schema ordering. Rejected: vocabulary dependencies and schema
dependencies are different concerns. A pack may consume another pack's vocabulary without
needing its tables created first.

### B. Auto-prefix on collision

If pack A declares `tasks` and pack B also declares `tasks`, auto-rename to `a_tasks` and
`b_tasks` silently. Rejected: hides bugs. Pack authors who genuinely meant the same table want
to know; pack authors who collided by accident need to fix it.

### C. Single shared schema_plan per backend

Operators declare one schema_plan per backend in TOML; packs don't own schemas. Rejected:
puts schema design in operators' hands when it should be pack-author's. Operators choose which
backend to use; pack authors choose what schema their pack needs.

### D. No collision check; first-write-wins

Apply each pack's plan; if a `CREATE TABLE IF NOT EXISTS` matches an existing table, assume
it's the same. Rejected: column-level schema drift would be silently accepted. The collision
check catches when two packs declared the same table name with different schemas.

## Consequences

### Positive

- Each pack ships its own migrations; no schema crowding in `khive-db`.
- New packs can declare their own tables without modifying `khive-db`.
- Per-pack version tracking — one pack's migration doesn't accidentally affect another's
  version counter.
- Backend isolation is preserved at the schema level (each backend's schema reflects only
  the packs assigned to it).
- Collision detection prevents silent schema drift when packs share a backend.

### Negative

- **Schema collision potential.** Two packs sharing a backend that declare the same table
  name = boot failure. Mitigation: the pack-name table prefix convention; clear collision
  error message with both pack names.
- **Pack authors must understand the substrate boundary.** Knowing whether a new table is
  "substrate" (lives in `khive-db`) or "pack extension" (lives in the pack) is an
  architectural judgment. This ADR documents the rule (substrate kinds = `khive-db`; anything
  else = pack); pack authors must apply it.
- **Declaration order is operator-visible.** Reordering packs in `khive.toml` may change
  schema-application order, which becomes part of the deployment contract.

### Neutral

- ADR-022's `VersionedMigration` mechanism applies identically per pack (no new versioning
  scheme).
- `ServiceSchemaPlan` type is unchanged from ADR-022.
- Shared substrate tables (`entities`, `notes`, etc.) continue to live in `khive-db` — no
  forced migration of existing tables into packs.

## Open Questions

1. **Per-backend extension support** — different backends might want different SQLite
   extensions loaded (sqlite-vec for vector-using backends, JSON1 for everywhere). Default v1:
   sqlite-vec loaded on every backend (cheap, idempotent). Per-backend extension lists are a
   future ADR if a real need emerges.
2. **Schema_plan downgrade / removal** — if a pack is removed from `khive.toml`, its tables
   stay in place (no destructive cleanup). Future: a `kkernel db prune` admin command. Not
   v1 scope.
3. **Cross-pack schema dependency** — if pack B's schema requires pack A's table to exist,
   should B declare `EXTENDS = ["A"]` for schema purposes? Reuse ADR-037's `REQUIRES` field
   or add a new field? Defer to operational evidence; v1 has no such cross-dependency.

## References

- ADR-022 — Schema migration mechanism (`ServiceSchemaPlan`, `VersionedMigration`)
- ADR-025 — Pack Standard (this ADR extends the Pack trait)
- ADR-037 — Inter-pack vocabulary dependencies (related but distinct from schema dependencies)
- ADR-079 — Pack-scoped backends (this ADR's parent context)
- ADR-080 / ADR-086 — Cross-backend operations (ADR-086's `target_backend` column migration is
  applied to all backends via the same `khive-db` substrate schema)
- khive-internal `crates/khive-lore/src/service.rs` — `SCHEMA_PLAN` const (the per-service-
  schema pattern this restores)
- `.khive/notes/plans/plan_20260522_runtime_restoration.md` Phase B3
