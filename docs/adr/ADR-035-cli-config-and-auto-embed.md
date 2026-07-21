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

### One config file, not two

ADR-028 introduces `khive.toml` for deployment topology: `[[backends]]`, `[[engines]]`, and
`[packs.*]` sections. This ADR originally specified a separate `config.toml` for embed and
schema settings. Having two configuration files in the same `.khive/` directory with
overlapping scopes is confusing and unnecessary. The decision is to **unify both into one
`khive.toml`** per scope, not two separate files.

The two files that ADR-028 and this ADR together define:

| File                  | Scope         | Committed                         |
| --------------------- | ------------- | --------------------------------- |
| `.khive/khive.toml`   | Project-level | Yes — shared across collaborators |
| `~/.khive/khive.toml` | User-level    | No — machine-specific             |

`khive.toml` is the single configuration file for khive at each scope level. There is no
`config.toml`. ADR-020's `.khive/.gitignore` allowlist includes `khive.toml`.

### The consistency requirement

Embedding vectors are only comparable if produced by the same model. If Alice commits with
`mE5-small` (384 dimensions) and Bob syncs with `BGE-small` (same dimension count but a
different model), their vectors are numerically incompatible: cosine similarity across models
is meaningless. The project-level `khive.toml` must specify the embedding model, and that
file must be committed so all collaborators use the same model.

This means:

- The embedding model is a **project-level setting** — committed to git, enforced across
  the team, not overridable per-user.
- Device preferences are **user-level settings** — machine-specific, not committed,
  reflecting local hardware (Metal, CUDA, CPU).

## Decision

### 1. Unified `khive.toml` — one file per scope

`khive.toml` carries all configuration for khive at a given scope. ADR-028's `[[backends]]`,
`[[engines]]`, and `[packs.*]` sections are joined by `[embed]` and `[schema]` sections from
this ADR. There is no separate `config.toml`.

**Project-level** (`.khive/khive.toml` — committed to git):

```toml
# .khive/khive.toml — project configuration
# Committed to git. All collaborators use these settings.
# See: ADR-028 (backends/packs) and ADR-035 (embed/schema).

# --- Backend and pack topology (ADR-028) ---

[[backends]]
name = "main"
path = "~/.khive/khive.db"
cache_mb = 256
journal_mode = "wal"

[[engines]]
name = "mE5-small"
dim = 384
weight = 1.0

[packs.kg]
backend = "main"
engines = ["mE5-small"]

[packs.memory]
backend = "main"
engines = ["mE5-small"]

[packs.gtd]
backend = "main"
engines = []

# --- Embedding configuration (ADR-035) ---

[embed]
model = "mE5-small"        # lattice-embed model name — must match [[engines]] entry
dimensions = 384            # vector dimensions
auto_embed = true           # embed on commit and sync (default: true)
batch_size = 64             # entities per embed batch

[embed.fields]
include = ["name", "description"]  # entity fields concatenated for embedding

# --- Schema validation (ADR-035) ---

[schema]
strict = true               # reject unknown entity kinds and edge relations on import
```

**User-level** (`~/.khive/khive.toml` — not committed):

```toml
# ~/.khive/khive.toml — user defaults, not committed to any project

[[backends]]
name = "main"
path = "~/.khive/khive.db"

[embed]
model = "mE5-small"        # default model for new projects
device = "metal"            # inference device: metal | cuda | cpu
```

Only keys that diverge from built-in defaults need to appear in either file.

### 2. Configuration resolution order

For every config key: **CLI flag > project `khive.toml` > global `khive.toml` > built-in
default**. A missing key at any level falls through to the next. When both levels specify
the same key, the **project level wins**. This ensures project maintainers can lock the
embedding model for consistency while users retain the ability to set their inference device
locally without touching committed files.

`embed.device` is user-level only. A project-level `khive.toml` that sets `embed.device`
is valid TOML but will be warned against at startup: device selection should not be committed.

## CLI / env / config precedence

For each runtime option, precedence is:
**CLI flag > project khive.toml > global khive.toml > `KHIVE_*` env var > built-in default**.
A `KHIVE_*` env var is only a fallback default — when a TOML key resolves at either level it
wins over the env var. This matches the runtime config loader (`engine_config.rs`) and the
config docs (`docs/khive-config-example.toml`).

Exception: **Loaded packs** resolves as `--pack` > `KHIVE_PACKS` > `runtime.packs` >
default — the environment outranks configuration for this option. Rationale and the
licensed-pack manifest constraint that bounds the resolved set:
[ADR-027 Amendment 3](ADR-027-dynamic-pack-loading.md).

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

**Configuration example** (`.khive/khive.toml`):

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

**Built-in defaults** (when no `khive.toml` is present):

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

### 4. `kkernel kg init` writes `.khive/khive.toml`

`kkernel kg init` writes `.khive/khive.toml` with the built-in defaults, making project
settings explicit and reviewable in PRs:

```toml
# .khive/khive.toml — project KG configuration
# Committed to git. All collaborators use these settings.

[[backends]]
name = "main"
path = "~/.khive/khive.db"
cache_mb = 256
journal_mode = "wal"

[[engines]]
name = "mE5-small"
dim = 384
weight = 1.0

[packs.kg]
backend = "main"
engines = ["mE5-small"]

[packs.memory]
backend = "main"
engines = ["mE5-small"]

[packs.gtd]
backend = "main"
engines = []

[embed]
model = "mE5-small"
dimensions = 384
auto_embed = true
batch_size = 64

[embed.fields]
include = ["name", "description"]

[schema]
strict = true
```

If `.khive/khive.toml` already exists, `init` does not overwrite it.

The `.khive/.gitignore` allowlist from ADR-020 adds `khive.toml` alongside `kg/`:

```gitignore
*
!.gitignore
!kg/
!kg/**
!khive.toml
```

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
# 1. Edit .khive/khive.toml:
#    embed.model = "BGE-large"
#    embed.dimensions = 1024

# 2. Re-embed all entities with the new model
kkernel kg embed --all

# 3. Commit the config change (vectors are local-only — only khive.toml changes in git)
kkernel kg commit -m "switch embedding model to BGE-large"
```

After the commit, other collaborators run:

```bash
git pull
kkernel kg sync     # rebuilds DB from NDJSON; auto-embeds with new model
```

`kkernel kg sync` reads the updated `.khive/khive.toml` after the DB rebuild step, so the
`embed_missing` pass uses the new model automatically.

### 8. Config validation

The CLI validates both `khive.toml` files at startup. Validation checks:

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

Unknown keys produce a warning but do not abort. This allows newer `khive.toml` shapes to
exist without breaking older `kkernel` versions.

A config parse error (malformed TOML, invalid value type) aborts with a structured message
that names the offending file and line:

```
ERROR: .khive/khive.toml line 5: expected integer for embed.dimensions, got "384px"
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

### Why one `khive.toml`, not two files

A separate `config.toml` alongside `khive.toml` in the same directory creates an
unnecessary split. Operators editing topology (`[[backends]]`) need to be in the same
mental context as operators editing embedding settings (`[embed]`). Merging both into
`khive.toml` reduces cognitive overhead, reduces the number of files the user must manage,
and produces a single committed file whose git diff shows the full project configuration
change.

The sections are orthogonal in structure (`[[backends]]` vs `[embed]`) and serve different
purposes (ADR-028 topology vs this ADR's embed pipeline), so there is no entanglement —
just cohabitation in one well-sectioned file.

### Why project config wins over global config

The embedding model is a project invariant. If a global `~/.khive/khive.toml` could
override the project's `embed.model`, a collaborator with a different default would silently
produce incompatible vectors. The project config must win on embedding-related keys.

`embed.device` is the only meaningful per-user override — it reflects local hardware. It
lives in the global config and does not affect the model selection that determines vector
compatibility.

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
| Separate `config.toml` alongside `khive.toml`          | Clear file roles            | Two files to manage; split mental context                             | One file per scope is simpler and sufficient                       |
| Single flat config (no two-level merge)                | Simplest model              | Cannot separate device (user) from model (project)                    | Model consistency across collaborators requires project-level lock |
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
- The embedding model is recorded in `.khive/khive.toml`, committed alongside the KG data.
  Changing the model produces a one-line diff in git that reviewers can see and approve.
- Device preferences stay local: `device = "metal"` never appears in committed files.
- `kkernel kg init` writes a well-commented `.khive/khive.toml` that makes all defaults
  explicit and reviewable in the initial PR.
- `kkernel kg embed --dry-run` gives visibility into which entities lack vectors before
  committing.
- One config file per scope, not two, reduces operator friction.

### Negative

- `kkernel kg commit` and `kkernel kg sync` have an optional embed step that adds latency.
  For large KGs on slow hardware, this may be noticeable. Mitigation: `auto_embed = false`
  moves embedding to an explicit `kkernel kg embed` call.
- `~/.khive/khive.toml` introduces user-level config that must be documented and
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
  artifacts beyond the `[embed]` and `[schema]` sections in `.khive/khive.toml`.
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
  `.khive/.gitignore` allowlist gains `khive.toml`
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; `[[backends]]`,
  `[[engines]]`, and `[packs.*]` sections live in the same `khive.toml` this ADR governs
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `EmbedderRegistry`; `embed_missing`
  routes inference requests through the registry; `embed.model` must reference a registered
  engine name
- [ADR-034](ADR-034-kg-validation-pipelines.md) — validation pipelines; embedding before
  export in `commit` allows validation rules to check vector presence and dimension
  correctness
