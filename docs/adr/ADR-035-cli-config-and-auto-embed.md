# ADR-035: CLI Configuration and Automatic Embedding

**Status**: accepted (amended 2026-08-01)
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

| File                   | Scope                               | Committed                         |
| ---------------------- | ----------------------------------- | --------------------------------- |
| `./khive.toml`         | Project-root compatibility location | Operator choice                   |
| `./.khive/config.toml` | Canonical project-local location    | Yes — shared across collaborators |
| `~/.khive/config.toml` | User-global fallback                | No — machine-specific             |

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
auto_embed = true           # design field; not wired by the current Rust CLI
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
> `./.khive/config.toml` > `~/.khive/config.toml` > no file**. The first file
> that exists is parsed and validated; a malformed higher-precedence file is an
> error, not a reason to continue to a lower tier.

When an explicit database path is supplied, the hidden project tier is
anchored beside that resolved database so a thin client and its daemon select
the same file. With no explicit database path, it is anchored to the current
project directory. This is the ADR-096 `config_id` coherence rule.

`kkernel mcp`, `kkernel exec` (including `--pending-events`), and `kkernel
reindex` all expose the explicit `--config` / `KHIVE_CONFIG` tier and thread it
through every post-resolution config reload. An entry point must not document
this tier while silently falling back to automatic discovery.

There is no per-key merge between project and global files. A machine-local
setting that must coexist with committed project settings uses the applicable
CLI or environment override.

## CLI / env / config precedence

For each runtime option, precedence is:
**CLI flag > selected config file > applicable `KHIVE_*` env var > built-in
default**. Exact option-specific exceptions are listed in the canonical config
reference (`docs/khive-config-example.toml`).

Pack selection is the exception specified by ADR-027 Amendment 3: `--pack` >
`KHIVE_PACKS` > `runtime.packs` > the built-in production set. Each layer replaces
the complete set; an empty layer falls through rather than selecting zero packs.

| Option             | CLI flag                         | Env var                             | Config key                | Default           |
| ------------------ | -------------------------------- | ----------------------------------- | ------------------------- | ----------------- |
| Namespace          | `--namespace`                    | `KHIVE_NAMESPACE`                   | `runtime.namespace`       | `default`         |
| Loaded packs       | `--pack` (repeat)                | `KHIVE_PACKS`                       | `runtime.packs`           | production set    |
| DB path            | `--db`                           | `KHIVE_DB`                          | `runtime.db_path`         | `~/.khive/kg.db`  |
| Recall min_score   | (n/a, per-call)                  | `KHIVE_RECALL_MIN_SCORE`            | `memory.recall.min_score` | `None` (no floor) |
| Disable embeddings | `kkernel mcp --no-embed`         | `KHIVE_NO_EMBED`                    | (none)                    | `false`           |
| Reindex model      | `kkernel reindex --model <name>` | `KHIVE_EMBEDDING_MODEL`             | `[[engines]]`             | built-in engine   |
| Additional models  | (none)                           | `KHIVE_ADDITIONAL_EMBEDDING_MODELS` | `[[engines]]`             | none              |
| Log level          | `--log-level`                    | `KHIVE_LOG`                         | `runtime.log_level`       | `info`            |
| Authorization gate | `--gate`                         | `KHIVE_GATE`                        | `runtime.gate`            | `allow-all`       |
| Brain profile      | `--brain-profile`                | `KHIVE_BRAIN_PROFILE`               | `runtime.brain_profile`   | `None`            |

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
3. **Pack-local tuning prior**: if neither explicit nor namespace-bound profile resolves, the
   pack-local in-memory state receives the update directly. `BalancedRecallState` retains the
   original memory-pack fallback. As amended by #1505, knowledge's `SectionPosteriorState` is
   keyed by the effective namespace so an explicit measurement arm cannot inherit live/local
   compose feedback. The default `local` path remains backward compatible.

This resolution is automatic: packs attempt tiers 1 and 2 silently and fall through to tier 3
when nothing is bound. No configuration is required for the default-namespace fallback to
continue working as before.

### 3. `[embed]` and `[schema]` sections

The original decision assigned automatic-pipeline settings to `[embed]` and import validation
to `[schema]`. The current Rust runtime resolves embedding engines from `[[engines]]`; it does
not read `embed.auto_embed`, and it exposes no `--auto-embed` or `KHIVE_AUTO_EMBED` control.
The shipped operational controls are `kkernel mcp --no-embed` / `KHIVE_NO_EMBED` and the
explicit `kkernel reindex` workflow in §5. The table below records the decision's intended
defaults, not additional live CLI flags.

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

### 5. Shipped embedding and repair workflow

The original decision specified an `embed_missing` pass in `kkernel kg commit` and
`kkernel kg sync`, plus a `kkernel kg embed` command. That automatic-embedding behavior and
dedicated command are not present in the current Rust CLI. `kkernel kg commit` validates and
commits a staged change-set; the current `kkernel kg sync` spelling is a visible alias for remote
fetch; and top-level `kkernel sync` rebuilds the SQLite database and FTS documents from NDJSON.
None invokes an embedding pass, and `kkernel kg` has no `embed` subcommand.

The shipped behavior has two parts:

1. Runtime create and update paths embed inline for every configured engine.
   `kkernel mcp --no-embed` (or `KHIVE_NO_EMBED=1`) starts the MCP runtime without any
   built-in embedding engine, so those writes remain text-only.
2. `kkernel reindex` is the explicit maintenance and repair command. It rebuilds vectors and
   FTS documents for entities, notes, and, by default, the knowledge corpus. It resolves the
   same database, config, namespace, and `[[engines]]` set as `kkernel mcp`.

Examples using only shipped flags:

```bash
# Re-embed entities and notes with every configured engine; knowledge uses the default.
kkernel reindex --db ~/.khive/khive.db --namespace local

# Repair only the graph substrate and keep vectors that already exist.
kkernel reindex --db ~/.khive/khive.db --namespace local \
  --no-knowledge --keep-existing

# Rebuild entity/note vectors with one named engine.
kkernel reindex --db ~/.khive/khive.db --namespace local \
  --no-knowledge --model all-minilm-l6-v2
```

Without `--keep-existing`, every staged record is re-embedded and each prior vector is
replaced atomically with its new value; a failed embed or insert leaves the prior vector in
place rather than deleting it first. With `--keep-existing`, records already embedded for
the selected model and namespace are skipped. FTS backfill still runs in either mode. The
default is fail-closed on partial failures; `--best-effort` is the explicit opt-in to a zero
exit after partial work.

There is no current `--embeds-only`, `--ids`, or `--dry-run` reindex mode. In particular,
`--keep-existing` means incremental vector top-up, not vector-only execution. When no
embedding engine is configured, `kkernel reindex` still backfills FTS but warns and skips
vector work. An operator who normally runs the server with `--no-embed` can therefore run a
separate `kkernel reindex` invocation without that server flag, using a config that declares
the desired `[[engines]]`, to populate vectors on an explicit schedule.

### 6. Embeddings are local-only derived state

Vectors are stored in `working.db` only. They are **not** written to NDJSON files and are
**not** committed to git. Three reasons:

- **Recomputable**: vectors are a deterministic function of the entity text and the
  embedding model. They carry no information beyond what `khive.toml` (model) and
  `entities.ndjson` (text) already record.
- **Size**: 384 floats per entity is 1.5 KB. A 10,000-entity KG would add 15 MB of
  non-human-readable binary content to NDJSON, destroying the git diff and merge
  guarantees that are the entire value of ADR-020.
- **Consistency**: `kkernel reindex` recomputes vectors from the selected engine set and
  current entity/note text. There is no durability requirement for the vectors themselves.

`working.db` is gitignored by ADR-020's allowlist. The `.khive/state/` directory is
ephemeral by design.

### 7. Model change workflow

When the project's embedding engine set changes, the existing vectors no longer describe the
selected configuration. The shipped workflow is:

```bash
# 1. Edit the selected config's [[engines]] entries.

# 2. Re-embed graph and knowledge state with that config.
kkernel reindex --config .khive/config.toml --db ~/.khive/khive.db \
  --namespace local

# 3. Commit only the config change; vectors remain local derived state.
git add .khive/config.toml
git commit -m "config: switch embedding engine"
```

After the commit, other collaborators run:

```bash
git pull
kkernel reindex --config .khive/config.toml --db ~/.khive/khive.db \
  --namespace local
```

### 8. Config validation

The original `[embed]` decision called for the following validation. The current Rust config
parser validates `[[engines]]` but does not deserialize these `[embed]` keys, so it does not
currently enforce this list:

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

The current Rust runtime does not consume `[embed]` or validate `embed.model` against that
registry. `[[engines]]` is the shipped source of truth: `default = true` selects the default
engine, inline create/update fans out across the registered set, and `kkernel reindex` does the
same for entities and notes unless `--model` narrows the pass. The `[embed]` shape above is
retained as the original decision record, not described as a second live selector.

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

The embedding engine set is a project invariant. If a global `~/.khive/config.toml` could
override the project's `[[engines]]`, a collaborator with a different default would silently
produce incompatible vectors. The project config must win on embedding-related keys.

Machine-local overrides such as device choice belong in an explicit CLI or
environment tier when a project config is selected. The global file is a
fallback for projects without a project config, not a merge source.

### Why inline writes plus an explicit repair command

Missing vectors degrade semantic search without producing a query error. The current runtime
therefore embeds ordinary create/update writes inline when engines are configured, while
`kkernel reindex` provides a deliberate full or incremental repair after bulk import, NDJSON
sync, or an engine change. Keeping reindex separate from git commit/sync also gives operators a
clear fail-closed maintenance command and avoids documenting lifecycle coupling the Rust CLI does
not implement.

### Why NDJSON files never carry vectors

Vectors in NDJSON would break the git-native positioning. A PR that updates an entity
description would also produce a 384-float vector diff that reviewers cannot interpret.
Merge conflicts on vector fields are semantically meaningless. The separation of committed
text (NDJSON) from derived local state (vectors in `working.db`) is the same principle
as separating source files from build artifacts in a standard software project.

## Alternatives Considered

| Alternative                                                 | Pros                         | Cons                                                                  | Why rejected                                                  |
| ----------------------------------------------------------- | ---------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------- |
| Separate active topology and embedding files in one project | Clear file roles             | Two files to manage; split mental context                             | One selected file is simpler and sufficient                   |
| Per-key project/global TOML merge                           | Machine-local overlays       | Hidden composite config; client/daemon fingerprint drift risk         | First-file selection is deterministic and auditable           |
| YAML config format                                          | Familiar                     | Ambiguous parsing; indentation errors in practice                     | TOML is unambiguous; already used in Cargo and this project   |
| JSON config format                                          | Machine-writable             | No comments; annoying to hand-edit; trailing-comma errors             | TOML is better for human-edited files                         |
| Vectors stored in NDJSON (committed)                        | Single source of truth       | 15 MB+ non-diffable content per 10K entities; breaks merge guarantees | Recomputable state should not be committed                    |
| Dedicated committed vector file (separate from NDJSON)      | Separates vectors from text  | Same merge problem; grows with entity count                           | Still recomputable; still breaks git diff                     |
| Manual repair only                                          | Explicit control             | Silent quality degradation when users forget                          | Inline create/update plus explicit reindex covers both paths  |
| Embed on every embedding-bearing write                      | Fresh vectors for new writes | Adds model latency to those writes                                    | Shipped default; `mcp --no-embed` is the explicit opt-out     |
| `embed.model` allowed as per-user override                  | User flexibility             | Incompatible vectors across collaborators                             | Model is a project invariant; must be locked at project level |

## Consequences

### Positive (amendment: brain profile knob)

- `memory.feedback` and `knowledge.feedback` can be directed to a specific brain profile
  through the same config path used by namespace — no per-call parameter needed.
- Deployments that bind a namespace to a brain profile via `brain.bind` benefit automatically
  from tier-2 resolution without any `khive.toml` change.
- The pack-local tuning prior (tier 3) continues without configuration. Memory behavior and the
  default knowledge namespace remain unchanged; explicit knowledge namespaces receive isolated
  fallback state instead of sharing local feedback (#1505).

### Positive

- `kkernel reindex` gives operators one verified command for full rebuilds and incremental
  top-up across the configured engine set.
- The embedding model is recorded in `.khive/config.toml`, committed alongside the KG data.
  Changing the model produces a one-line diff in git that reviewers can see and approve.
- Device preferences stay local: `device = "metal"` never appears in committed files.
- `kkernel kg init` writes a valid, well-commented `.khive/config.toml` that makes its defaults
  explicit and reviewable in the initial PR.
- `--keep-existing` avoids recomputing vectors already present for the selected model and
  namespace.
- One selected config file, not a hidden per-key merge, reduces operator friction.

### Negative

- Inline create/update embedding adds model latency. On model-less or latency-sensitive servers,
  `kkernel mcp --no-embed` disables that work and a separate `kkernel reindex` process can run on
  an explicit schedule.
- `~/.khive/config.toml` introduces a user-global fallback that must be documented and
  supported. Model availability errors from lattice-embed are propagated at runtime.
- Changing `[[engines]]` requires re-embedding the affected corpus (potentially slow for large
  KGs). The explicit workflow is documented in §7.

### Neutral

- The NDJSON files and their git history are unchanged. This ADR adds no new committed
  artifacts beyond the selected config sections in `.khive/config.toml`.
- `working.db` already carries a per-(model, dim) vector table layout (ADR-005, ADR-009).
  This ADR specifies when those tables are populated, not how they are structured.
- Projects that do not use semantic retrieval can run `kkernel mcp --no-embed`; text search
  remains available, and `kkernel reindex` without a configured engine still backfills FTS.

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
- [ADR-005](ADR-005-storage-capability-traits.md) — `VectorStore` trait; `kkernel reindex`
  writes to per-(model, dim) tables via this trait
- [ADR-009](ADR-009-backend-architecture.md) — `khive-db` backend works in-memory and
  on-disk; `working.db` is a project-scoped on-disk backend
- [ADR-011](ADR-011-embedding-and-inference.md) — lattice-embed boundary used for batched
  reindex inference
- [ADR-020](ADR-020-git-native-kg-implementation.md) — git-native KG implementation and the
  NDJSON sync boundary; the `.khive/.gitignore` allowlist gains `config.toml`
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; `[[backends]]`,
  `[[engines]]`, and `[packs.*]` sections live in the same selected TOML file this ADR governs
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `EmbedderRegistry`; `kkernel reindex`
  fans entity/note work across registered engines unless `--model` narrows it
- [ADR-034](ADR-034-kg-validation-pipelines.md) — validation pipelines remain separate from
  the explicit reindex maintenance path
