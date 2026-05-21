# ADR-054: KG Schema Evolution

**Status**: accepted (Phase E1 — `khive kg migrate` with add_kind / rename_kind / remove_kind (error|migrate_to) / add_property / rename_property / remove_property / add_relation_endpoint / remove_relation_endpoint (error|drop) implemented in Deno CLI; --dry-run, --to, --list flags + sequence-gap detection + atomic per-migration apply. Phase E2 — change_property_type with coerce deferred. Phase E3 — schema diff helpers + on-pull compatibility checks deferred)\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 introduced `schema.yaml` as the ontology manifest for a project's KG: it declares the
entity kinds, edge relations, property schemas, and remote references in use. The `format_version`
field in that ADR tracks which version of the `schema.yaml` file format is in use (what top-level
keys are valid), not a version counter for the graph data itself.

That definition was correct for v1.0.0 of the format but left a gap: schemas evolve. A team that
started with `concept`, `document`, `project`, `person`, `org` may add a `benchmark` kind six months
later. ADR-050 packs add vocabulary to `schema.yaml` declaratively. Property schemas gain new
required keys. Edge endpoints are tightened or extended. Cross-repo remotes are bumped to new
commits.

When this happens, three problems arise that ADR-048 did not address:

1. **Existing entities may become invalid.** An entity whose `kind` was valid under the previous
   schema may not be valid under the new one if a kind was renamed or removed. `khive kg validate`
   will reject it with no path forward.

2. **Collaborators may have different schemas.** When two developers pull from the same repo and one
   has installed an additional pack (via ADR-050) while the other has not, `khive kg validate`
   produces different results on the same NDJSON files. There is no mechanism to compare, negotiate,
   or auto-merge schema differences.

3. **Pack lifecycle touches schema.** ADR-050 defines `khive pack install` and `khive pack remove`
   but does not specify what happens to the ontology version, whether a migration is required
   before removal, or how pack-added kinds interact with the entity corpus.

This ADR defines:

1. `ontology_version`: a new `schema.yaml` field that evolves under schema changes (semver semantics)
2. A migration system: declarative YAML operations in `.khive/kg/migrations/`
3. Compatibility rules for pull and merge operations that encounter schema differences
4. How pack installation and removal interact with schema versioning
5. Backward compatibility guarantees for entities created under older schema versions
6. `khive kg schema diff` for inspecting schema differences between branches or remotes

### Relationship to other ADRs

- **ADR-048**: Defines `schema.yaml` format and introduces the `format_version` field (file format
  compatibility). This ADR introduces `ontology_version` as a separate field tracking schema
  evolution. The two fields coexist: `format_version` signals parser compatibility,
  `ontology_version` signals entity-level compatibility.
- **ADR-050**: Defines declarative packs and their `pack.yaml` format. This ADR defines how pack
  installation and removal bump `schema.yaml`'s `ontology_version` and require or produce migrations.
- **ADR-053**: Defines KG branching. Migrations are git-tracked and must be applied on branch
  checkout when the target branch has migrations the current branch does not.
- **ADR-022**: Defines the SQLite migration system (`VersionedMigration` in `khive-db`). This ADR
  is a parallel system at the ontology layer, not at the storage layer. The two do not interact.

## Decision

### 1. Schema Versioning Semantics

`schema.yaml` uses two distinct version fields:

- **`format_version`** (introduced by ADR-048): the file format compatibility version. Consumers
  use this to determine whether they can parse the file. The Deno CLI and Rust runtime reject
  `schema.yaml` files whose `format_version` they do not understand.
- **`ontology_version`** (introduced by this ADR): the schema evolution version — what entity
  kinds, edge endpoint rules, and property schemas are declared. `khive kg validate` uses this
  to determine whether pending migrations exist.

`ontology_version` uses semver (`MAJOR.MINOR.PATCH`) with the following semantics:

| Change type    | Version bump | Examples                                                                                                           |
| -------------- | ------------ | ------------------------------------------------------------------------------------------------------------------ |
| Breaking       | Major        | Removing an entity kind, removing an edge relation, changing a required property type, renaming a kind or relation |
| Additive       | Minor        | Adding a new entity kind, adding an optional property, adding a new pack, relaxing an endpoint rule                |
| Non-functional | Patch        | Updating descriptions, adding documentation comments, bumping a remote commit SHA                                  |

**Breaking changes require a migration file** (see §2) before `khive kg validate` will accept the
new schema against the existing NDJSON corpus. The CLI enforces this: if the `ontology_version`
increases by a major version and no migration file covers the transition, `validate` exits with:

```
ERROR: ontology_version major bump from 1.0.0 to 2.0.0 with no migration for this transition.
  Add a migration to .khive/kg/migrations/ or run 'khive kg schema migrate --dry-run' to preview
  the generated migration for common operations.
```

**Additive changes do not require a migration.** An entity created under `ontology_version` v1.0
whose `kind` is still present in v1.1 remains valid. The `khive kg validate` command accepts
entities created under any minor version within the current major series.

**Patch bumps are transparent.** No validation behavior changes.

The `ontology_version` field in a freshly initialized schema (from `khive kg init`) is `"1.0.0"`.
The khive CLI that wrote the schema is recorded in a `khive_version` field:

```yaml
format_version: "1.1.0" # file format compatibility (ADR-048)
ontology_version: "1.2.0" # schema evolution semver (this ADR)
khive_version: "0.4.1" # khive CLI version that last wrote this file
```

`format_version` is used by parsers to determine whether they understand the file structure.
`ontology_version` is used by `khive kg validate` and `khive kg migrate` to determine entity
compatibility and pending migrations.
`khive_version` is informational — it is not used for validation. It helps debugging when a
schema file was written by an older CLI.

### 2. Migration System

Migrations live in `.khive/kg/migrations/` as YAML files, ordered by filename prefix:

```
.khive/kg/migrations/
  0001_add_benchmark_kind.yaml
  0002_rename_training_to_run.yaml
  0003_remove_legacy_experiment_kind.yaml
```

Filenames use a four-digit zero-padded sequence number followed by a short description. The
sequence number is the application order. Gaps in the sequence are errors.

#### Migration file format

```yaml
version_from: "1.0.0"
version_to: "1.1.0"
description: "Add benchmark kind and paper_url property to concept"
operations:
  - add_kind:
      name: benchmark
      description: "Evaluation benchmark or dataset used to measure model performance"

  - add_property:
      kind: concept
      name: paper_url
      type: string
      required: false
      description: "Canonical URL for the paper (arXiv, DOI, ACL Anthology)"
```

```yaml
version_from: "1.1.0"
version_to: "2.0.0"
description: "Rename training_run to run; remove legacy experiment kind"
operations:
  - rename_kind:
      from: training_run
      to: run

  - remove_kind:
      name: experiment
      on_existing: error # "error" | "migrate_to" + target
```

#### Supported operations

| Operation                  | Arguments                                                            | Effect on NDJSON                                                                                                                                   |
| -------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `add_kind`                 | `name`, `description?`                                               | None (NDJSON already valid; new kind becomes accepted)                                                                                             |
| `remove_kind`              | `name`, `on_existing: error\|migrate_to`                             | If `migrate_to`: rewrites matching entity lines to target kind. If `error`: aborts if any entities of this kind exist                              |
| `rename_kind`              | `from`, `to`                                                         | Rewrites all matching `"kind": "<from>"` lines in `entities.ndjson`                                                                                |
| `add_property`             | `kind`, `name`, `type`, `required`, `description?`                   | None if `required: false`. If `required: true`, aborts unless all entities of that kind already have the property                                  |
| `remove_property`          | `kind`, `name`                                                       | No rewrite required; previously set values are retained in NDJSON but no longer schema-validated                                                   |
| `rename_property`          | `kind`, `from`, `to`                                                 | Rewrites `properties.<from>` to `properties.<to>` in matching entity lines                                                                         |
| `change_property_type`     | `kind`, `name`, `from_type`, `to_type`, `coerce?`                    | If `coerce: true` and type coercion is defined: rewrites property values. If `coerce: false` (default): aborts if any entity has this property set |
| `add_relation_endpoint`    | `relation`, `source_kind`, `target_kind`                             | None (NDJSON already valid; new endpoint pair becomes accepted)                                                                                    |
| `remove_relation_endpoint` | `relation`, `source_kind`, `target_kind`, `on_existing: error\|drop` | If `drop`: removes matching edges from `edges.ndjson`. If `error`: aborts if any such edges exist                                                  |

All operations are applied in order within a migration file. A migration is atomic: if any
operation fails, no NDJSON changes are written and `schema.yaml`'s `ontology_version` is not updated.

#### Applying migrations

```bash
khive kg migrate          # apply all pending migrations in sequence
khive kg migrate --dry-run  # show what would change without writing
khive kg migrate --to 1.2.0  # apply migrations up to a specific version
```

`khive kg migrate` determines pending migrations by comparing `schema.yaml`'s current
`ontology_version` against the `version_from` of each migration file. Migrations whose
`version_from` is less than the current `ontology_version` have already been applied and are
skipped. The command applies migrations in filename order until all migrations have been processed
or until the target version is reached.

After a successful migration, `schema.yaml`'s `ontology_version` is updated to the final
`version_to` and the file is written. The NDJSON files are also written if any operations rewrote
entity or edge lines. These changes should be committed:

```bash
khive kg migrate
git add .khive/kg/
git commit -m "chore(kg): migrate schema from 1.1.0 to 2.0.0"
```

Migrations are git-tracked alongside the NDJSON data. The full schema history — what versions the
project passed through and what changes each migration made — is visible in `git log
.khive/kg/migrations/`.

### 3. Schema Compatibility on Pull and Merge

When pulling or merging a branch that has a different `schema.yaml` than the current branch,
`khive kg` compares the `ontology_version` fields and selects one of three outcomes:

#### Patch or minor version difference (additive merge)

If the incoming `schema.yaml`'s `ontology_version` is a minor or patch bump relative to the
current (i.e., only additive changes), git merges `schema.yaml` automatically. After the git
merge, `khive kg validate` confirms no integrity violations were introduced. No manual
intervention is required.

If both branches incremented the minor `ontology_version` independently (e.g., current has
`1.1.0` from adding `benchmark`, incoming has `1.1.0` from adding `training_run`), git may
conflict on the `ontology_version` field. In this case:

```bash
khive kg schema merge-resolve
```

resolves the conflict by computing a new minor version that includes both additions:

1. Takes the union of `entity_kinds`, `edge_relations`, `properties`, and `packs` from both
   versions.
2. Increments the minor `ontology_version` to the next unused value.
3. Writes the merged `schema.yaml`.
4. The user commits the resolved file.

#### Major version difference (explicit migration required)

If the incoming schema has a higher major `ontology_version` than the current branch, the merge
is refused:

```
ERROR: cannot auto-merge ontology_version 2.0.0 (incoming) with 1.3.0 (current).
  Major version bump requires explicit migration.
  Run 'khive kg migrate' on the current branch to reach 2.0.0, then retry the merge.
```

The user must apply the migrations on their branch first, advancing their `schema.yaml`'s
`ontology_version` to the same major version as the incoming branch, before the merge proceeds.
This prevents silent data loss from entities that would become invalid under the new major schema.

#### Incompatible schemas with no migration path (diverged)

If the schemas have diverged in ways that no migration file covers (e.g., two packs that define
the same kind name with different semantics), `khive kg validate --schema-compat <branch>` reports
the specific conflicts:

```
CONFLICT: entity kind 'benchmark' defined in both schemas with different property rules.
  Current:  required property 'dataset_url'
  Incoming: required property 'paper_url'
  Resolution: edit schema.yaml manually to reconcile, or file a pack compatibility issue.
```

#### `khive kg validate --schema-compat`

```bash
khive kg validate --schema-compat <branch>    # compare current schema to a branch
khive kg validate --schema-compat <remote>:<sha>  # compare to a remote schema at a commit
```

This command does not touch any data files. It reports:

- Kinds present in one schema but not the other
- Property key conflicts (same key, different types or required/optional status)
- Endpoint rules present in one schema but not the other
- Pack version differences

Exit code 0 means the schemas are compatible (additive merge is safe). Non-zero means manual
resolution is required.

### 4. Pack Integration with Schema Evolution

Pack installation and removal are schema-changing operations. They interact with the migration
system and the `ontology_version` field.

#### Installing a pack

`khive pack install <pack>` (defined in ADR-050) does the following to `schema.yaml`:

1. Appends the pack's declared `entity_kinds`, `note_kinds`, and `edge_endpoints` to the
   appropriate sections.
2. Adds the pack to `schema.yaml`'s `packs` section (introduced by ADR-050):
   ```yaml
   packs:
     - name: ml-papers
       version: "1.0.0"
       source: ohdearquant/khive-pack-ml-papers
   ```
3. Increments the minor `ontology_version` (additive change).

No migration file is required — pack installation is always additive. The new kinds and endpoints
become accepted by `khive kg validate` immediately after install.

#### Removing a pack

`khive pack remove <pack>` is a potentially breaking operation. The CLI checks whether any
entities of the pack-added kinds exist in the corpus:

```bash
khive pack remove ml-papers
```

If entities of `model`, `benchmark`, or `training_run` exist, the CLI refuses:

```
ERROR: cannot remove pack 'ml-papers' — 42 entities of kind 'model' exist.
  Options:
    --migrate-to concept    re-kind all matching entities to 'concept' (creates a migration)
    --dry-run               show affected entities without removing
```

If `--migrate-to <kind>` is given, the CLI generates a migration file that renames all matching
entity kinds to the target kind, and adds it to `.khive/kg/migrations/`. It then increments the
major `ontology_version` (because kinds are being removed from the schema). The user applies the
migration and commits.

Pack removal without `--migrate-to` always fails when affected entities exist, enforcing the
backward compatibility rule that data is never silently dropped.

#### Atomic `remove_pack` operation

`khive pack remove` alone is **insufficient** for pack-owned kinds when entities of those kinds
exist. The correct procedure is the `remove_pack` atomic operation sequence, which ensures
`schema.yaml` and the NDJSON corpus remain in a consistent state:

1. **Data migration**: apply a migration that renames all entities of pack-owned kinds to a
   surviving kind (using `rename_kind` or `remove_kind` with `on_existing: migrate_to`).
2. **Remove pack entry**: remove the pack from `schema.yaml#packs`.
3. **Recompute merged vocabulary**: update `entity_kinds`, `note_kinds`, and edge endpoint
   sections to reflect the removal.
4. **Version bump**: increment the major `ontology_version` (kind removal is a breaking change).

These four steps are executed atomically by `khive pack remove --migrate-to <kind>`. If any step
fails, none of the `schema.yaml` changes are written. Running `khive pack remove` without
`--migrate-to` (when entities of pack kinds exist) only performs the check and error reporting
— it does not modify any files.

The `remove_kind` migration operation alone is **not sufficient** for pack-owned kinds: if the
pack entry remains in `schema.yaml#packs`, the next `khive pack validate` or pack vocabulary
recompute will re-add the kind. The pack entry must be removed in the same operation that removes
the kind from the corpus.

#### Pack upgrades

When a pack's `version` advances (e.g., from `1.0.0` to `1.1.0`), the pack author publishes a
migration YAML alongside the pack. `khive pack upgrade <pack>` downloads the migration, adds it
to `.khive/kg/migrations/`, and runs `khive kg migrate`. Pack upgrade migrations follow the same
operation format as user-authored migrations.

If the pack upgrade is a major version bump (breaking changes to pack vocabulary), the same
major-version rules apply: the user must apply migrations explicitly and may need to resolve
conflicts.

Pack versions are tracked in `schema.yaml#packs[*].version`. `khive pack list` shows installed
packs with their current and available versions.

### 5. Backward Compatibility Rules

The following guarantees hold across schema versions:

1. **Entities created under schema vN.x.y remain valid under vN.x+k.y** (same major version,
   higher minor or patch). Additive changes do not invalidate existing entities.

2. **Entities may not be valid under vN+1.x.y without migration.** Major version bumps may
   remove or rename kinds. `khive kg validate` rejects entities whose kind is not present in
   the current schema and reports them with their IDs and current kinds.

3. **Migrations must be explicit.** No migration is ever applied automatically during a pull,
   merge, or import. The user runs `khive kg migrate` deliberately. This preserves the
   invariant that data changes are always tracked in git as intentional commits.

4. **Data is never silently dropped.** Operations that would remove entities (`remove_kind`
   with `on_existing: error`) abort rather than proceed. `remove_kind` with `on_existing:
   migrate_to` rewrites entity kinds rather than deleting records.

5. **`khive kg validate` always reports the full set of violations.** It does not stop at the
   first error. The report groups violations by type (unknown kind, unknown relation, missing
   required property, referential integrity failure) to help the user understand the scope of
   migration needed.

6. **Entity compatibility is determined by `ontology_version` in `schema.yaml` and the applied
   migration sequence.** NDJSON entity records do not carry a per-entity version field.
   Compatibility is a property of the corpus as a whole: if the current `ontology_version` accepts
   a kind, all entities of that kind are valid. Entities whose kind was valid under a prior
   `ontology_version` but has since been removed or renamed are flagged by `khive kg validate`
   as unknown-kind violations — the resolution is to run the pending migrations.

### 6. Schema Diffing

```bash
khive kg schema diff                      # compare working tree schema to last commit
khive kg schema diff <branch>             # compare current branch to another branch
khive kg schema diff HEAD~5               # compare current to 5 commits ago
khive kg schema diff main..feat/ml-vocab  # compare two branches
```

Output format:

```
Schema diff: main..feat/ml-vocab

+ entity_kind: benchmark
  description: "Evaluation benchmark or dataset"

+ entity_kind: training_run
  description: "A single training experiment run"

~ concept properties:
  + paper_url (string, optional)
  + domain.values: added "fine-tuning", "quantization"

~ edge_relation: depends_on
  + endpoint: (training_run, dataset)

+ pack: ml-papers @ 1.0.0

  ontology_version: 1.0.0 → 1.2.0
```

`khive kg schema diff` is a presentation layer over `git diff schema.yaml` — it parses both YAML
versions and renders the difference in ontology-aware terms rather than raw YAML line diffs. It
does not modify any files.

The diff output is also available as structured JSON (`--format json`) for programmatic consumers
(CI scripts, frontend schema comparison views).

#### CI integration

The CI workflow generated by `khive kg init` (ADR-048 §6) is extended to run `khive kg schema
diff HEAD~1` on every push that modifies `schema.yaml`, printing the ontology diff as a workflow
annotation. PRs that modify `schema.yaml` show the diff in the PR summary, giving reviewers a
clear view of what vocabulary is being added or removed.

If the push advances the major version, the workflow additionally checks that a migration file
covering the transition is present and passes `khive kg migrate --dry-run` to confirm the
migration is syntactically valid:

```yaml
- name: Schema evolution check
  if: ${{ steps.changed-files.outputs.schema_yaml == 'true' }}
  run: |
    khive kg schema diff HEAD~1
    khive kg migrate --dry-run
```

## Rationale

### Why semver for ontology versioning

Semver communicates intent clearly: major means breaking, minor means additive, patch means
non-functional. Tool authors, pack authors, and CI scripts can make automated decisions based on
the version bump magnitude without parsing the full diff. The semantics are already familiar to
the Rust and npm ecosystems that khive's users inhabit.

Using an integer counter (v1, v2, v3) would lose the breaking/additive distinction without
additional metadata. Using a date-based version (2026-05-20) would make compatibility comparisons
require date arithmetic. Semver is the right fit.

### Why migrations are explicit rather than auto-applied

Auto-applying migrations on pull or checkout would mean that opening a branch causes data rewrites
without a commit. This violates the core ADR-048 principle that NDJSON files are git-managed: a
`git checkout` should not silently rewrite files that are not in the checkout target's tree.

Explicit migrations preserve the invariant that every data change is a deliberate commit with a
clear message. The user runs `khive kg migrate`, sees what changed, and commits. The git log
records precisely when each migration was applied and what changed.

The cost is user friction: the user must run a command before their corpus is valid under the new
schema. The benefit is auditability and safety: no silent data loss, no surprise rewrites on
checkout.

### Why not auto-generate migrations from schema diffs

Schema diffs can be ambiguous. If `training_run` disappears and `run` appears in the same
`schema.yaml`, did the author rename the kind or remove one and add an unrelated other? An
auto-generated migration cannot know the intent. A human-authored migration explicitly states
`rename_kind: {from: training_run, to: run}`, which is unambiguous and can be applied
deterministically.

The CLI provides `khive kg schema migrate --dry-run` which inspects the diff and proposes a
plausible migration as a starting point. The user reviews and edits before committing. This
combines automation assistance with human intent verification.

### Why pack removal requires explicit migration

A pack adds vocabulary that may have been used to create hundreds of entities. Silently removing
the pack vocabulary from `schema.yaml` would leave those entities in a state that `khive kg
validate` rejects. This would mean a collaborator who has not installed the pack cannot validate
the corpus, creating a fragmented state.

Requiring an explicit migration (`--migrate-to <kind>`) ensures the corpus is always in a state
that is valid under the current schema. The majority use case (removing a pack with no entities
of its kinds) proceeds without friction; only the edge case (entities exist of removed-pack
kinds) requires a migration.

### Why `on_existing: error` is the default for `remove_kind`

The default should prevent silent data loss. If a user runs `remove_kind` without considering
the existing data, they should receive an error that names the problem and their options, not a
silent drop. `migrate_to` is the safe path; `error` is the safe default.

## Alternatives Considered

| Alternative                                        | Reason rejected                                                                                                                                         |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No migrations, validate-only                       | Leaves users with invalid entities and no automated path forward; requires manual NDJSON surgery                                                        |
| Auto-migrate on pull or checkout                   | Violates git-native principle; causes silent data rewrites not tracked as commits                                                                       |
| Auto-generate migrations from diffs                | Schema diffs are ambiguous; cannot distinguish rename from remove+add; unsafe as default                                                                |
| SQL-style migrations (numbered imperative scripts) | YAML declarative operations are more auditable, tool-parseable, and reversible; SQL approach is for the storage layer (ADR-022), not the ontology layer |
| Schema-less (no validation)                        | Defeats the purpose of a typed KG; errors surface as query failures, not validation failures                                                            |
| Integer sequence version (v1, v2)                  | Loses breaking/additive distinction; forces consumers to parse full diff to understand impact                                                           |
| Pack removal always forbidden if entities exist    | Too restrictive; blocks legitimate cleanup; `--migrate-to` provides a safe path                                                                         |

## Consequences

### Positive

- Schema changes are trackable in git with clear commit messages and diffs.
- Breaking changes are gated behind explicit user action (migration), preventing accidental data
  loss.
- `khive kg schema diff` gives reviewers an ontology-level view of PR changes, not raw YAML diffs.
- Pack lifecycle is cleanly integrated: install bumps minor version, removal requires migration
  when entities exist.
- Collaborators can compare schema compatibility before attempting a merge, catching incompatible
  ontologies early.
- CI catches missing migrations before a PR lands.

### Negative

- Schema evolution now requires user action (running `khive kg migrate`) when major versions
  bump. This is additional friction compared to the current state where there is no migration
  system.
- Migration files must be authored (or reviewed after auto-generation) for any breaking change.
  This is deliberate friction — breaking changes should require deliberate action.
- Pack removal with existing entities of removed-pack kinds requires a two-step process: write a
  migration, apply it, then remove the pack. This is more work than a simple `khive pack remove`.

### Neutral

- `schema.yaml` gains a new `ontology_version` field alongside the existing `format_version` and
  `khive_version` fields. The two version fields serve distinct purposes: `format_version` signals
  to parsers whether they can read the file; `ontology_version` signals to validators whether
  migrations are pending. Existing `schema.yaml` files without `ontology_version` treat that field
  as `"1.0.0"` (the initial baseline, consistent with no migrations applied).
- `khive-vcs` gains a `migrate.rs` module alongside `schema.rs`, `export.rs`, `import.rs`, and
  `validate.rs`. Migration execution is part of the VCS crate, not a separate crate.
- ADR-022's SQLite migration system is unchanged. The two systems are parallel and independent:
  ADR-022 manages storage schema (table definitions); this ADR manages ontology schema (entity
  kinds and properties). They do not share sequence numbers or coordination.

## Implementation

### Crate changes

`crates/khive-vcs/` gains:

```
crates/khive-vcs/
└── src/
    ├── migrate.rs     — MigrationFile, MigrationOp enum, apply_migration(), pending_migrations()
    ├── schema.rs      — SchemaYaml extended with packs section; ontology_version bump helpers
    └── diff.rs        — schema_diff(): SchemaYaml × SchemaYaml → SchemaDiff struct
```

`SchemaDiff` is a structured type (not a string) that can be rendered as text or JSON. The
diff renderer lives in `diff.rs`; the CLI command formats it via `Display`.

### New CLI commands

```
khive kg migrate [--dry-run] [--to <version>]
khive kg schema diff [<ref>] [--format text|json]
khive kg validate --schema-compat <ref>
khive kg schema merge-resolve
```

These commands join the existing `init`, `export`, `import`, `validate`, `diff`, `update`
commands in the Deno CLI (`deno/src/kg/`).

### `schema.yaml` additions

This ADR adds the `ontology_version` field to `schema.yaml`. The `format_version` and
`khive_version` fields are from ADR-048 and remain unchanged. The `packs` section from ADR-050
is the only structural addition to `schema.yaml`. No other top-level keys are added.

A fully annotated `schema.yaml` header after this ADR:

```yaml
format_version: "1.1.0" # file format version (ADR-048) — bumped when the schema.yaml
#   structure itself gains new top-level keys
ontology_version: "2.0.0" # ontology evolution version (this ADR) — bumped when kinds,
#   relations, or properties change
khive_version: "0.4.1" # CLI version that last wrote this file (informational)
```

### Migration directory

`khive kg init` creates `.khive/kg/migrations/` as an empty directory and adds a `.gitkeep` file
so the directory is tracked by git. The CI workflow is updated to validate migration sequence
integrity (no gaps, no duplicate sequence numbers).

### Phasing

| Phase | Scope                                                                                              | Target |
| ----- | -------------------------------------------------------------------------------------------------- | ------ |
| 1     | `MigrationFile` parser + `add_kind` / `rename_kind` / `remove_kind` operations; `khive kg migrate` | v0.5   |
| 2     | `add_property` / `remove_property` / `rename_property` operations                                  | v0.5   |
| 3     | `schema_diff()` + `khive kg schema diff` command                                                   | v0.5   |
| 4     | `validate --schema-compat` + `schema merge-resolve`                                                | v0.6   |
| 5     | `change_property_type` with coerce support; pack upgrade migration pipeline                        | v0.6   |
| 6     | CI workflow extension (schema diff annotations, migration dry-run gate)                            | v0.6   |

Phase 1 covers the primary use case (adding and renaming kinds) and is independently shippable.
Phases 4-6 are quality-of-life features that improve the collaborative workflow but are not
required for solo use.

## References

- ADR-022: Schema Migrations (SQLite storage layer migrations; parallel and independent)
- ADR-048: Git-Native KG Versioning (defines `schema.yaml` format and `format_version` field)
- ADR-050: Declarative Pack Format and Local Pack Management (pack install/remove lifecycle)
- ADR-052: KG Storage Model (working.db and NDJSON committed snapshot; migration applies to both)
- ADR-053: KG Branching and Merge (branch checkout must handle pending migrations; see §3)
- `crates/khive-vcs/src/schema.rs` — `SchemaYaml` type extended by this ADR
- `crates/khive-vcs/src/validate.rs` — `validate()` function extended to check migration gap
