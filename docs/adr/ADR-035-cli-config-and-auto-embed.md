# ADR-035: CLI Configuration and Automatic Embedding

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

ADR-020 defines the `.khive/` directory layout, the NDJSON format, and the `kkernel kg commit`
and `kkernel kg sync` pipelines. ADR-028 defines pack-scoped backend assignment via
`khive.toml`. Neither ADR addresses two practical concerns that affect every project using
the git-native KG workflow:

1. **Runtime configuration**: the embedding model, device preferences, and schema strictness
   are hard-coded defaults. Different projects may need different models; different machines
   have different inference hardware. There is no way to record these settings alongside the
   KG data or override them at the user level without recompiling.

2. **Automatic embeddings**: `kkernel search` uses hybrid FTS + vector search (ADR-012).
   Vectors must exist in `working.db` for the vector component to contribute. Without
   automatic embedding on commit and sync, vectors grow stale and search quality degrades
   silently — no error, just worse results.

### One config schema and one selected file

ADR-028 introduces the khive TOML schema for deployment topology:
`[[backends]]`, `[[engines]]`, and `[packs.*]` sections. This ADR adds embed and
schema settings to that same schema; it does not create a second sidecar file
with an overlapping purpose.

The accepted loader discovers these filenames, in precedence order:

| File                      | Scope                                  | Committed                         |
| ------------------------- | -------------------------------------- | --------------------------------- |
| `./khive.toml`            | Project-root compatibility location    | Operator choice                   |
| `./.khive/config.toml`    | Canonical project-local location       | Yes — shared across collaborators |
| `~/.khive/config.toml`    | User-global fallback                   | No — machine-specific             |

Only the first existing file is loaded. The files are not merged per key.
`.khive/khive.toml` is not a discovery tier and must never be silently treated
as one. ADR-020's `.khive/.gitignore` allowlist includes `config.toml`.

### The consistency requirement

Embedding vectors are only comparable if produced by the same model. If Alice commits with
`all-minilm-l6-v2` (384 dimensions) and Bob syncs with `BGE-small` (same dimension count but a
different model), their vectors are numerically incompatible: cosine similarity across models
is meaningless. The project-level `.khive/config.toml` (or accepted root
`khive.toml` alternative) must specify the embedding model, and that file must
be committed so all collaborators use the same model.

This means:

- The embedding model is a **project-level setting** — committed to git, enforced across
  the team, not overridable per-user.
- Device preferences are **machine-local settings** — supplied through a
  process-local override rather than a second TOML file merged behind the
  selected project config.

## Decision

### 1. Unified TOML schema

The selected config file carries all configuration for khive. ADR-028's
`[[backends]]`, `[[engines]]`, and `[packs.*]` sections are joined by
`[embed]` and `[schema]` sections from this ADR.

**Canonical project-level file** (`.khive/config.toml` — committed to git):

```toml
# .khive/config.toml — project configuration
# Committed to git. All collaborators use these settings.
# See: ADR-028 (backends/packs) and ADR-035 (embed/schema).

# --- Backend and pack topology (ADR-028) ---

[[backends]]
name = "main"
kind = "sqlite"
path = "~/.khive/khive.db"

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
dims = 384

[packs.kg]
backend = "main"

[packs.memory]
backend = "main"

[packs.gtd]
backend = "main"

# --- Embedding configuration (ADR-035) ---

[embed]
model = "default"          # logical name — must match a [[engines]] entry
dimensions = 384            # vector dimensions
auto_embed = true           # embed on commit and sync (default: true)
batch_size = 64             # entities per embed batch

[embed.fields]
include = ["name", "description"]  # entity fields concatenated for embedding

# --- Schema validation (ADR-035) ---

[schema]
strict = true               # reject unknown entity kinds and edge relations on import
```

**User-global fallback** (`~/.khive/config.toml` — not committed):

```toml
# ~/.khive/config.toml — user defaults, not committed to any project

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
```

Only keys that diverge from built-in defaults need to appear. The global file
is selected only when neither project location exists; it is not merged into a
selected project file.

### 2. Configuration resolution order

Configuration-file discovery is **explicit `--config` / `KHIVE_CONFIG` path
> project-root `./khive.toml` > DB-anchored or cwd project
`./.khive/config.toml` > `~/.khive/config.toml` > no file**. The first file
that exists is parsed and validated; a malformed higher-precedence file is an
error, not a reason to continue to a lower tier.

When an explicit database path is supplied, the hidden project tier is
anchored beside that resolved database so a thin client and its daemon select
the same file. With no explicit database path, it is anchored to the current
project directory. This is the ADR-096 `config_id` coherence rule.

There is no per-key merge between project and global files. A machine-local
setting that must coexist with committed project settings uses the applicable
CLI or environment override.

## CLI / env / config precedence

For each runtime option, precedence is:
**CLI flag > selected config file > applicable `KHIVE_*` env var > built-in
default**. Exact option-specific exceptions are listed in the canonical config
reference (`docs/core/khive-config-example.toml`).

| Option             | CLI flag          | Env var                  | Config key                | Default           |
| ------------------ | ----------------- | ------------------------ | ------------------------- | ----------------- |
| Namespace          | `--namespace`     | `KHIVE_NAMESPACE`        | `runtime.namespace`       | `default`         |
| Loaded packs       | `--pack` (repeat) | `KHIVE_PACKS`            | `runtime.packs`           | `kg`              |
| DB path            | `--db`            | `KHIVE_DB`               | `runtime.db_path`         | `~/.khive/kg.db`  |
| Recall min_score   | (n/a, per-call)   | `KHIVE_RECALL_MIN_SCORE` | `memory.recall.min_score` | `None` (no floor) |
| Auto-embed mode    | `--auto-embed`    | `KHIVE_AUTO_EMBED`       | `embed.auto_embed`        | `true`            |
| Embedding model    | `--embed-model`   | `KHIVE_EMBED_MODEL`      | `embed.model`             | `mE5-small`       |
| Log level          | `--log-level`     | `KHIVE_LOG`              | `runtime.log_level`       | `info`            |
| Authorization gate | `--gate`          | `KHIVE_GATE`             | `runtime.gate`            | `allow-all`       |
| Brain profile      | `--brain-profile` | `KHIVE_BRAIN_PROFILE`    | `runtime.brain_profile`   | `None`            |

Note: `recall(min_score)` has **no floor by default**. Operators serving larger corpora should
set `KHIVE_RECALL_MIN_SCORE=0.5` (or similar) in production deployments.

### Brain profile configuration

The `brain_profile` option designates which brain profile receives feedback from
`memory.feedback` and `knowledge.feedback`, and from which profile recall-time score
boosting reads. It is configured the same way namespace is — via `--brain-profile`,
`KHIVE_BRAIN_PROFILE`, or `runtime.brain_profile` in `khive.toml`.

**Configuration example** (`.khive/config.toml`):

```toml
[runtime]
namespace = "local"
brain_profile = "project-recall-v1"
```

**Feedback and recall-boost profile resolution order** (for `memory.feedback`,
`knowledge.feedback`, and recall-time boosting):

1. **Explicit profile in config**: if `runtime.brain_profile` / `KHIVE_BRAIN_PROFILE` /
   `--brain-profile` resolves to a non-empty string, that profile ID is used directly.
2. **Namespace-bound profile**: if no explicit profile is set but a namespace is configured,
   the feedback handler calls `brain.resolve(consumer_kind="recall")` for that
   namespace and uses the resolved profile.
3. **Global tuning prior**: if neither explicit nor namespace-bound profile resolves, the
   pack-local in-memory state (`BalancedRecallState` for memory, `SectionPosteriorState` for
   knowledge) receives the update directly. This is the intended global fallback — it is
   not a bug.

This resolution is automatic: packs attempt tiers 1 and 2 silently and fall through to tier 3
when nothing is bound. No configuration is required for the global-prior behavior to continue
working as before.

### 3. `[embed]` and `[schema]` sections

The `[embed]` section controls the automatic embedding pipeline (§5). The `[schema]`
section controls import validation.

**Built-in defaults** (when no selected config file supplies the setting):

| Key                    | Default                   |
| ---------------------- | ------------------------- |
| `embed.model`          | `mE5-small`               |
| `embed.dimensions`     | `384`                     |
| `embed.auto_embed`     | `true`                    |
| `embed.batch_size`     | `64`                      |
| `embed.fields.include` | `["name", "description"]` |
| `embed.device`         | `cpu`                     |
| `schema.strict`        | `true`                    |

`embed.fields.include` specifies which entity fields are concatenated to produce the
embedding input. `name` and `description` are the canonical top-level entity fields. Any
other string is treated as a key under the entity's `properties` map. The reserved
discriminant `kind` is explicitly forbidden — it is a closed-taxonomy tag (ADR-001), not
an embeddable text field.

### 4. `kkernel kg init` writes `.khive/config.toml`

`kkernel kg init` writes a minimal, valid `.khive/config.toml` that pins the
default embedding engine in the schema the accepted loader consumes:

```toml
# .khive/config.toml — project KG configuration
# Committed to git. All collaborators use these settings.

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
dims = 384
```

If `.khive/config.toml` already exists, `init` does not overwrite it. The
non-overwrite guarantee uses an atomic create rather than an existence check
followed by a truncating write. If root `khive.toml` already exists, init
preserves that accepted higher-precedence config and does not create a hidden
file that the loader would ignore.

`.khive/khive.toml` is the obsolete initializer spelling and is not a loader
tier. When it exists, init fails before writing scaffolding and names both the
legacy and canonical paths. When legacy and canonical files both exist, init
fails without modifying either one; the operator must reconcile them
explicitly.

The `.khive/.gitignore` allowlist from ADR-020 adds `config.toml` alongside
`kg/`:

```gitignore
*
!.gitignore
!kg/
!kg/**
!config.toml
```

Init automatically updates only the byte-exact `.gitignore` emitted by the
old initializer (`!khive.toml` to `!config.toml`). It never rewrites a
customized ignore file.

### 5. Automatic embedding pipeline

Embeddings are generated during the two operations that transition working state: commit
and sync. Both use the same `embed_missing` subroutine.

#### `embed_missing` subroutine

Queries `working.db` for entities that have no vector in the per-(model, dim) virtual table
for the currently configured model, or whose vector was computed with a different model than
the current `embed.model`. Constructs the input text by joining the values of
`embed.fields.include` with a single space separator. Calls lattice-embed (ADR-011) in
batches of `embed.batch_size` via the EmbedderRegistry (ADR-031). Writes resulting vectors
to the appropriate per-(model, dim) table in `working.db` via the VectorStore trait
(ADR-005).

For an entity with `name = "LoRA"` and `description = "Low-rank adaptation technique for
fine-tuning"`, the concatenated input is:
`"LoRA Low-rank adaptation technique for fine-tuning"`.

#### `kkernel kg commit` — embed before export

The `commit` pipeline from ADR-020 §6 is extended:

1. Run `embed_missing` on `working.db` if `embed.auto_embed = true`.
2. Run `kkernel kg export` (DB → NDJSON). Unchanged from ADR-020.
3. Run `kkernel kg validate`. Unchanged from ADR-020.
4. `git add .khive/kg/` and `git commit`. Unchanged from ADR-020.

Embedding runs before export because export reads `working.db`; per-entity validation
rules (ADR-034) that check vector quality must see vectors already present. Embedding after
export would make such checks impossible without a second pass.

#### `kkernel kg sync` — embed after rebuild

The `sync` pipeline from ADR-020 §6 is extended:

1. Check for uncommitted DB changes. Unchanged from ADR-020.
2. Atomic DB rebuild from NDJSON. Unchanged from ADR-020.
3. Run `embed_missing` on the freshly rebuilt DB if `embed.auto_embed = true`. Embeds
   entities that arrived from other collaborators and lack local vectors.
4. Print summary: `Synced: 472 entities, 1,111 edges (38 entities embedded)`.

Embedding runs after rebuild because the rebuild drops and recreates `working.db` from
NDJSON. Embedding before rebuild would populate vectors into a DB that is immediately
discarded.

#### `kkernel kg embed` — explicit command

An explicit command for full or selective re-embedding:

```
kkernel kg embed              # embed all entities missing vectors for current model
kkernel kg embed --all        # re-embed all entities (force, regardless of existing vectors)
kkernel kg embed --ids a1b2 c3d4  # embed specific entity IDs
kkernel kg embed --dry-run    # print which entities would be embedded; no writes
```

When `auto_embed = false`, `kkernel kg embed` is the only way embeddings are created.
Projects that want explicit control (large KGs, separate embed jobs, slow hardware) set
`auto_embed = false` and call `kkernel kg embed` on their own schedule.

### 6. Embeddings are local-only derived state

Vectors are stored in `working.db` only. They are **not** written to NDJSON files and are
**not** committed to git. Three reasons:

- **Recomputable**: vectors are a deterministic function of the entity text and the
  embedding model. They carry no information beyond what `khive.toml` (model) and
  `entities.ndjson` (text) already record.
- **Size**: 384 floats per entity is 1.5 KB. A 10,000-entity KG would add 15 MB of
  non-human-readable binary content to NDJSON, destroying the git diff and merge
  guarantees that are the entire value of ADR-020.
- **Consistency**: `kkernel kg sync` re-embeds after every rebuild. Two collaborators
  using the same model and entity text produce identical vectors. There is no durability
  requirement.

`working.db` is gitignored by ADR-020's allowlist. The `.khive/state/` directory is
ephemeral by design.

### 7. Model change workflow

When the project's embedding model changes, all vectors in `working.db` are incompatible
with the new model. The workflow is:

```bash
# 1. Edit .khive/config.toml:
#    embed.model = "BGE-large"
#    embed.dimensions = 1024

# 2. Re-embed all entities with the new model
kkernel kg embed --all

# 3. Commit the config change (vectors are local-only — only config.toml changes in git)
kkernel kg commit -m "switch embedding model to BGE-large"
```

After the commit, other collaborators run:

```bash
git pull
kkernel kg sync     # rebuilds DB from NDJSON; auto-embeds with new model
```

`kkernel kg sync` reads the updated `.khive/config.toml` after the DB rebuild step, so the
`embed_missing` pass uses the new model automatically.

### 8. Config validation

The CLI validates the one selected config file at startup. Validation checks:

- `embed.model` is a non-empty string. Model availability is validated by lattice-embed
  at runtime; the config loader does not check against a list.
- `embed.dimensions` is a positive integer.
- `embed.batch_size` is a positive integer.
- `embed.fields.include` is a non-empty array of strings. Each string must be `name`,
  `description`, or a key that will be looked up in `entity.properties` at embed time.
  The reserved discriminant `kind` is forbidden.
- `schema.strict` is a boolean.
- `embed.device` (global config only) is one of `metal`, `cuda`, `cpu`.
- `[[backends]]` and `[[engines]]` sections are validated per ADR-028.

Unknown keys produce a warning but do not abort. This allows newer config shapes to
exist without breaking older `kkernel` versions.

A config parse error (malformed TOML, invalid value type) aborts with a structured message
that names the offending file and line:

```
ERROR: .khive/config.toml line 5: expected integer for embed.dimensions, got "384px"
```

### 9. Relationship between `[embed]` and `[[engines]]`

`[[engines]]` (ADR-028) declares the process-wide registry of loaded embedding models —
the names and dimensions that `EmbedderRegistry::from_config` uses to instantiate models.

`[embed]` (this ADR) specifies which model is used for the entity-text embedding pipeline
and what fields it operates on. `embed.model` must reference a name in `[[engines]]`. The
runtime validates this at startup:

```
ERROR: embed.model "BGE-large" not found in [[engines]]. Available: mE5-small
```

This separation keeps the registry declaration (ADR-028) orthogonal to embed pipeline
configuration (this ADR). A deployment can load multiple engines (for query-time
multi-engine retrieval, ADR-031) while designating exactly one as the entity embedding
model for the commit/sync pipeline.

## Rationale

### Why one selected config, not a merged pair

Two simultaneously active files in the same project create an unnecessary
split. Operators editing topology (`[[backends]]`) need to be in the same
mental context as operators editing embedding settings (`[embed]`). One
selected file reduces cognitive overhead and produces a single committed diff
that shows the full project configuration change.

The sections are orthogonal in structure (`[[backends]]` vs `[embed]`) and serve different
purposes (ADR-028 topology vs this ADR's embed pipeline), so there is no entanglement —
just cohabitation in one well-sectioned file.

### Why project config wins over the global fallback

The embedding model is a project invariant. If a global `~/.khive/config.toml` could
override the project's `embed.model`, a collaborator with a different default would silently
produce incompatible vectors. The project config must win on embedding-related keys.

Machine-local overrides such as device choice belong in an explicit CLI or
environment tier when a project config is selected. The global file is a
fallback for projects without a project config, not a merge source.

### Why auto-embed defaults to true

Without automatic embedding, the user-visible symptom of stale vectors is worse search
results, not an error. There is no "search returned poor results because vectors are
missing" warning — the user sees a lower-quality result set and does not know why.
Auto-embedding prevents this failure mode by ensuring vectors are current after every
commit and sync. The cost is a few seconds of embed time, negligible for typical KG sizes.
`auto_embed = false` is the explicit opt-out for large KGs or slow hardware.

### Why embed before export in `kkernel kg commit`

Embedding before export allows validation rules (ADR-034) to check vector quality — for
example, flagging entities whose stored embedding dimension does not match the configured
model's output. Embedding after export would make such pre-commit checks impossible.

### Why embed after rebuild in `kkernel kg sync`

The rebuild drops and recreates `working.db`. Embedding before rebuild populates vectors
into a database that is immediately discarded. Embedding after rebuild ensures vectors are
computed against the final, committed entity set.

### Why NDJSON files never carry vectors

Vectors in NDJSON would break the git-native positioning. A PR that updates an entity
description would also produce a 384-float vector diff that reviewers cannot interpret.
Merge conflicts on vector fields are semantically meaningless. The separation of committed
text (NDJSON) from derived local state (vectors in `working.db`) is the same principle
as separating source files from build artifacts in a standard software project.

## Alternatives Considered

| Alternative                                            | Pros                        | Cons                                                                  | Why rejected                                                       |
| ------------------------------------------------------ | --------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Separate active topology and embedding files in one project | Clear file roles       | Two files to manage; split mental context                             | One selected file is simpler and sufficient                        |
| Per-key project/global TOML merge                      | Machine-local overlays      | Hidden composite config; client/daemon fingerprint drift risk         | First-file selection is deterministic and auditable                |
| YAML config format                                     | Familiar                    | Ambiguous parsing; indentation errors in practice                     | TOML is unambiguous; already used in Cargo and this project        |
| JSON config format                                     | Machine-writable            | No comments; annoying to hand-edit; trailing-comma errors             | TOML is better for human-edited files                              |
| Vectors stored in NDJSON (committed)                   | Single source of truth      | 15 MB+ non-diffable content per 10K entities; breaks merge guarantees | Recomputable state should not be committed                         |
| Dedicated committed vector file (separate from NDJSON) | Separates vectors from text | Same merge problem; grows with entity count                           | Still recomputable; still breaks git diff                          |
| Manual embed only (`auto_embed = false` as default)    | Explicit control            | Silent quality degradation when users forget                          | Auto-embed prevents the failure mode at negligible cost            |
| Embed on every verb write (real-time)                  | Vectors always current      | Embed latency per write blocks interactive use                        | Batch on commit/sync matches the git-workflow cadence              |
| `embed.model` allowed as per-user override             | User flexibility            | Incompatible vectors across collaborators                             | Model is a project invariant; must be locked at project level      |

## Consequences

### Positive (amendment: brain profile knob)

- `memory.feedback` and `knowledge.feedback` can be directed to a specific brain profile
  through the same config path used by namespace — no per-call parameter needed.
- Deployments that bind a namespace to a brain profile via `brain.bind` benefit automatically
  from tier-2 resolution without any `khive.toml` change.
- The global tuning prior (tier 3) continues to work unchanged for deployments that do not
  configure a profile. No existing behavior is removed.

### Positive

- Search quality is reliable: every collaborator who runs `kkernel kg sync` or `kkernel kg
  commit` has current vectors without manual intervention.
- The embedding model is recorded in `.khive/config.toml`, committed alongside the KG data.
  Changing the model produces a one-line diff in git that reviewers can see and approve.
- Device preferences stay local: `device = "metal"` never appears in committed files.
- `kkernel kg init` writes a valid, well-commented `.khive/config.toml` that makes its defaults
  explicit and reviewable in the initial PR.
- `kkernel kg embed --dry-run` gives visibility into which entities lack vectors before
  committing.
- One selected config file, not a hidden per-key merge, reduces operator friction.

### Negative

- `kkernel kg commit` and `kkernel kg sync` have an optional embed step that adds latency.
  For large KGs on slow hardware, this may be noticeable. Mitigation: `auto_embed = false`
  moves embedding to an explicit `kkernel kg embed` call.
- `~/.khive/config.toml` introduces a user-global fallback that must be documented and
  supported. A misconfigured `embed.device` produces a runtime error from lattice-embed
  rather than a config validation error. Mitigation: type validation catches `device`
  value errors at startup; model availability errors from lattice-embed are propagated
  with their full message.
- Changing `embed.model` requires re-embedding all entities (potentially slow for large
  KGs) and a follow-up commit. The workflow is documented in §7 but adds ceremony to
  model upgrades.
- `embed.model` must match a name in `[[engines]]`. Operators who add a new model must
  update both sections consistently. Mitigation: startup validation reports the mismatch
  with the list of available engine names.

### Neutral

- The NDJSON files and their git history are unchanged. This ADR adds no new committed
  artifacts beyond the selected config sections in `.khive/config.toml`.
- `working.db` already carries a per-(model, dim) vector table layout (ADR-005, ADR-009).
  This ADR specifies when those tables are populated, not how they are structured.
- Projects that do not use `kkernel search` can set `auto_embed = false` and ignore the
  embed subsystem entirely. The pipeline steps are no-ops when `auto_embed = false` and
  `kkernel kg embed` is never invoked.

## Open Questions

1. **`[embed.fields.include]` as a pack-level field.** For packs with non-standard entity
   schemas (e.g., a `lore` pack where atoms have `title` + `body` instead of `name` +
   `description`), a global `[embed.fields]` is too coarse. A future iteration may move
   embed field configuration under `[packs.*.embed_fields]`. The `[embed.fields]` section
   in this ADR is the v1 baseline for the common case; pack-level overrides are deferred.

2. **Per-namespace model selection.** Multi-namespace deployments may eventually need
   different models per namespace. `embed.model` is a single project-wide setting in this
   ADR. Namespace-scoped model selection is deferred until a real use case requires it.

3. **`embed.dimensions` validation against the actual model.** At startup, the CLI could
   call lattice-embed to query the model's output dimension and compare it to
   `embed.dimensions`. This would catch mismatches early. Deferred: requires the embed
   runtime to be loaded even when no embedding is needed (e.g., `kkernel kg status`),
   which adds startup latency. Log a mismatch warning on first embed instead.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md) — `embed.fields.include` cannot include
  `kind`; it is a closed-taxonomy discriminant, not an embeddable text field
- [ADR-005](ADR-005-storage-capability-traits.md) — `VectorStore` trait; `embed_missing`
  writes to per-(model, dim) tables via this trait
- [ADR-009](ADR-009-backend-architecture.md) — `khive-db` backend works in-memory and
  on-disk; `working.db` is a project-scoped on-disk backend
- [ADR-011](ADR-011-embedding-and-inference.md) — lattice-embed boundary; `embed_missing`
  calls lattice-embed for batched inference
- [ADR-020](ADR-020-git-native-kg-implementation.md) — git-native KG implementation;
  this ADR extends the `commit` and `sync` pipelines defined in ADR-020 §6; the
  `.khive/.gitignore` allowlist gains `config.toml`
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; `[[backends]]`,
  `[[engines]]`, and `[packs.*]` sections live in the same selected TOML file this ADR governs
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `EmbedderRegistry`; `embed_missing`
  routes inference requests through the registry; `embed.model` must reference a registered
  engine name
- [ADR-034](ADR-034-kg-validation-pipelines.md) — validation pipelines; embedding before
  export in `commit` allows validation rules to check vector presence and dimension
  correctness
