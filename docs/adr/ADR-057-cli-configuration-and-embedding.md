# ADR-057: CLI Configuration and Automatic Embedding

**Status**: accepted (Phase C1 — `lib/config.ts`, `khive kg config`, `khive kg embed` plan, and commit/sync embed-plan banner implemented; Phase C2 — `lattice-embed` runtime wiring deferred until the Rust embedding binary is available)\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 defines the `.khive/kg/` NDJSON format and CLI commands (`init`, `export`, `import`,
`validate`). ADR-051 defines `khive kg commit` and `khive kg sync`. Neither ADR addresses two
practical concerns that affect every user of the git-native KG workflow:

1. **Runtime configuration**: the embedding model, device preferences, and schema strictness
   are currently hard-coded defaults. Different projects may need different models; different
   machines may have different inference hardware. There is no way to record these settings
   in the repo alongside the KG data, or to override them at the user level without recompiling.

2. **Automatic embeddings**: `khive search` uses hybrid FTS + vector search (ADR-012). Vectors
   must exist in `working.db` for the vector component to contribute. Without automatic
   embedding on commit and sync, users must remember to invoke an embed step manually — they
   forget, vectors grow stale, and search quality degrades silently with no visible error.

### The consistency requirement

Embedding vectors are only comparable if produced by the same model. If Alice commits with
`mE5-small` (384 dimensions) and Bob syncs with `BGE-small` (384 dimensions), their vectors are
numerically incompatible: cosine similarity between vectors from different models is meaningless.
The project-level configuration must specify the embedding model, and that file must be committed
so all collaborators use the same model.

This means:

- The embedding model is a **project-level setting** — committed to git, shared across
  the team, not overridable per-user.
- Device preferences and API endpoints are **user-level settings** — specific to the machine,
  not committed, reflecting local hardware (Metal, CUDA, CPU).

### What changes and what does not

- ADR-048 (`export`, `import`, `validate`) and ADR-051 (`commit`, `sync`): unchanged in
  their external behavior. This ADR specifies _when_ embedding runs within those operations,
  not how they work.
- ADR-052 (`working.db` schema, `.state/` layout): unchanged. Embeddings are stored in
  `working.db` via the sqlite-vec extension already present in the schema (ADR-009).
- lattice-embed crate (local inference): consumed as-is. This ADR specifies only which model
  name and device the crate receives, not how it works.

## Decision

### 1. Two-level TOML configuration

The CLI resolves configuration from two TOML files, merged at runtime.

**Project-level** (committed to git, shared across all collaborators):

```
.khive/config.toml
```

This file is part of the `.khive/` directory layout defined in ADR-048 §1. The `.khive/.gitignore`
uses an allowlist that explicitly includes `config.toml` alongside `kg/` — all other `.khive/`
contents (state/, notes/, discover/, etc.) are gitignored by default.

**User-level** (not committed, machine-specific defaults):

```
~/.khive/config.toml
```

Resolution order: **CLI flag > project config > global config > built-in default**.

A missing key at any level falls through to the next. If both levels specify the same key,
the project level wins (a project override beats a user default). This ordering ensures that
project maintainers can lock the embedding model for consistency, while users retain the
ability to set their inference device without touching committed files.

### 2. Configuration schema

**Project-level (`.khive/config.toml`)** — committed to git:

```toml
# .khive/config.toml

[embed]
model = "mE5-small"             # lattice-embed model name
dimensions = 384                # vector dimensions
auto_embed = true               # embed on commit and sync (default: true)
batch_size = 64                 # entities per embed batch

[embed.fields]
include = ["name", "description"]  # entity fields concatenated for embedding

[schema]
strict = true                   # --schema-mode strict on import (default: true)
```

**Global (user-level) (`~/.khive/config.toml`)** — not committed:

```toml
# ~/.khive/config.toml

[embed]
model = "mE5-small"             # default model for new projects
device = "metal"                # inference device: metal | cuda | cpu

[auth]
api_url = "https://api.khive.ai"  # khive.ai API endpoint (overrides ADR-051 default)
```

Only keys that diverge from the built-in defaults need to be present in either file.

**Built-in defaults** (if no config file is present):

| Key                    | Default                   |
| ---------------------- | ------------------------- |
| `embed.model`          | `mE5-small`               |
| `embed.dimensions`     | `384`                     |
| `embed.auto_embed`     | `true`                    |
| `embed.batch_size`     | `64`                      |
| `embed.fields.include` | `["name", "description"]` |
| `schema.strict`        | `true`                    |
| `embed.device`         | `cpu`                     |
| `auth.api_url`         | `https://api.khive.ai`    |

### 3. Why TOML

TOML is the standard configuration format in the Rust and Deno ecosystems (Cargo.toml, Deno
config). It is human-readable, supports inline comments, has no ambiguous block/flow style
distinction (unlike YAML), and no trailing-comma issues (unlike JSON). It is the right format
for human-edited config files. JSON is the right format for machine-written interchange data.
YAML is ambiguous enough that two parsers may disagree on the same file. TOML is deterministic.

### 4. `khive kg init` creates `.khive/config.toml`

`khive kg init` writes `.khive/config.toml` with the built-in defaults so the committed
defaults are explicit and reviewable in PRs:

```toml
# .khive/config.toml — project KG configuration
# Committed to git. All collaborators use these settings.
# See: https://khive.ai/docs/adr/ADR-057

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

If `.khive/config.toml` already exists, `init` does not overwrite it.

### 5. Automatic embedding pipeline

Embeddings are generated during the two operations that transition the working state: commit
and sync. Both use the same `embed_missing` subroutine.

#### `embed_missing` subroutine

Queries `working.db` for entities that have no vector in the `entities_vec` virtual table (or
whose vector was computed with a different model than the current config). Calls lattice-embed
in batches of `embed.batch_size` with the text produced by concatenating the configured
`embed.fields.include` fields, separated by a single space. Writes the resulting vectors back
to `working.db`.

For the configured `mE5-small` model, the concatenated text for an entity with `name = "LoRA"`
and `description = "Low-rank adaptation technique for fine-tuning"` would be:
`"LoRA Low-rank adaptation technique for fine-tuning"`.

#### `khive kg commit` — embed before validate

The `commit` pipeline defined in ADR-051 §4 is extended:

1. Run `embed_missing` (embed any entities in `working.db` that lack vectors). Only runs if
   `embed.auto_embed = true` in the resolved config.
2. Run `khive kg export` (DB → NDJSON files). Unchanged from ADR-051.
3. Run `khive kg validate`. Unchanged from ADR-051.
4. `git add .khive/kg/` and `git commit`. Unchanged from ADR-051.

Embedding happens before export because `export` reads the DB; the vectors are in `working.db`,
not in the NDJSON files (see §6). The NDJSON export is unaffected by this step.

#### `khive kg sync` — embed after rebuild

The `sync` pipeline defined in ADR-051 §5 is extended:

1. Check for changes (unchanged from ADR-051).
2. Atomic DB rebuild from NDJSON (unchanged from ADR-051).
3. Run `embed_missing` on the freshly rebuilt DB. Embeds entities that arrived from other
   collaborators and have no local vectors yet. Only runs if `embed.auto_embed = true`.
4. Print summary: `Synced: 472 entities, 1,111 edges (38 entities embedded)`.

Embedding happens after rebuild because the rebuild produces the DB from which vectors are
derived. The ordering ensures every entity in the synced DB has a vector by the time the user
runs their first search.

#### `khive kg embed` — explicit command

An explicit command for full or selective re-embedding:

```
khive kg embed              # embed all entities missing vectors
khive kg embed --all        # re-embed all entities (force, regardless of existing vectors)
khive kg embed --ids a1b2 c3d4  # embed specific entity IDs
khive kg embed --dry-run    # print which entities would be embedded, no writes
```

`khive kg embed` is also the command to run after changing `embed.model` in the project config,
to re-embed all entities with the new model. It should be followed by `khive kg commit`.

If `embed.auto_embed = false`, this is the only way embeddings are created. Projects that want
explicit control over when embedding runs (e.g., to batch it separately from commits in large
KGs) set `auto_embed = false` and call `khive kg embed` on their own schedule.

### 6. Embeddings are local-only derived state

Vectors are stored in `working.db` only. They are **not** written to the NDJSON files and are
**not** committed to git. Rationale:

- Vectors are a derivative of the entity text and the embedding model. They carry no information
  that is not already captured by the entity fields and the `embed.model` config key.
- Vectors are large (384 floats per entity = 1.5KB). A 10K-entity KG would add 15MB of
  non-human-readable binary content to NDJSON files, breaking the diff and merge guarantees
  that are the entire value proposition of ADR-048.
- Vectors can always be recomputed from the NDJSON data and the configured model. `khive kg sync`
  rebuilds them automatically after every checkout. There is no durability requirement.
- Two contributors with the same model config will produce identical vectors for the same entity
  text, so there is no loss from discarding vectors on sync and recomputing them locally.

`working.db` is gitignored (ADR-052 §1). The `.state/` directory is ephemeral by design.

### 7. Model change workflow

When the project's embedding model changes (edit `embed.model` in `.khive/config.toml`), all
existing vectors in `working.db` were computed with the old model and are incompatible with the
new model's output. The workflow is:

```bash
# 1. Update the model in project config
#    (edit .khive/config.toml: embed.model = "BGE-large")

# 2. Re-embed all entities with the new model
khive kg embed --all

# 3. Commit the config change (vectors are local-only — only the config changes in git)
khive kg commit -m "switch embedding model to BGE-large"
```

After `khive kg commit` pushes the config change, other collaborators run:

```bash
git pull
khive kg sync           # rebuilds DB from NDJSON, then auto-embeds with new model
```

`khive kg sync` reads the updated `.khive/config.toml` after the DB rebuild step, so it
automatically uses the new model for the `embed_missing` pass.

### 8. Config validation

The CLI validates both config files on startup against a built-in schema. Validation checks:

- `embed.model` is a non-empty string (not checked against a list; model availability is
  validated by lattice-embed at runtime).
- `embed.dimensions` is a positive integer.
- `embed.batch_size` is a positive integer.
- `embed.fields.include` is a non-empty array of strings. Each string must be a valid entity
  field name: `name` and `description` are canonical top-level fields; any other string is
  treated as a key in the entity `properties` map (the runtime reads it from
  `entity.properties` at embed time). The reserved discriminant `kind` is explicitly
  forbidden — it is a closed-taxonomy tag (ADR-001), not an embeddable text field.
  Note: `embed.fields.include` is an array and cannot be set via `khive kg config set`;
  edit `.khive/config.toml` directly to change it.
- `schema.strict` is a boolean.
- `embed.device` (global config only) is one of `metal`, `cuda`, `cpu`.

Unknown keys produce a warning but do not abort. This allows newer versions of the config
schema to be present in `.khive/config.toml` without breaking older CLI versions.

A config parse error (malformed TOML, invalid value type) aborts the CLI with a structured
error that names the offending file and line:

```
ERROR: .khive/config.toml line 5: expected integer for embed.dimensions, got "384px"
```

## Rationale

### Why project config beats global config

The embedding model is a project invariant. If a global default could override the project
config, a collaborator whose `~/.khive/config.toml` specifies a different model would silently
produce incompatible vectors. The project config must win on embedding-related keys.

The device setting (`embed.device`) is the only meaningful per-user override — it reflects the
hardware available on a given machine (Apple Silicon vs. CUDA GPU vs. CPU-only). The global
config is the right place for it, and it does not override the project-level model selection.

### Why auto-embed defaults to true

Without automatic embedding, the user-visible symptom is degraded search results — not an
error message. There is no "search returned wrong results because vectors are missing" warning;
there is just a lower-quality result set. Auto-embedding prevents this failure mode silently by
ensuring vectors are always current. The cost is a few seconds of embed time per commit and sync,
which is negligible for typical graph sizes.

Users who want explicit control (large KGs, slow hardware, separate embed jobs) can set
`auto_embed = false`.

### Why embed before validate in `khive kg commit`

Embedding before validate allows a future validation rule (ADR-056) to check vector quality —
for example, flagging entities whose embedding dimension does not match the configured model's
output. Embedding after validate would make such checks impossible without a second pass.

### Why embed after rebuild in `khive kg sync`

The rebuild in `sync` drops and recreates `working.db` from NDJSON. Embedding before rebuild
would fill vectors into a DB that is immediately discarded. Embedding after rebuild ensures
vectors are computed against the final, committed entity set, not a transitional state.

### Why NDJSON files do not carry vectors

Vectors in NDJSON would break the "GitHub for knowledge graphs" positioning. A PR that updates
an entity's description would also show a 384-float vector diff that reviewers cannot parse.
Merge conflicts on vector fields are meaningless. The git diff value proposition requires
human-readable NDJSON, and vector data is not human-readable. The separation of committed data
(NDJSON) from derived local state (vectors in `working.db`) is analogous to git's separation
of source files from build artifacts.

## Alternatives Considered

| Alternative                               | Pros                                | Cons                                                                                                     | Why rejected                                                         |
| ----------------------------------------- | ----------------------------------- | -------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Single flat config (no two-level merge)   | Simpler mental model                | Cannot separate device preferences (user-level) from model selection (project-level)                     | Model consistency across collaborators requires project-level config |
| YAML config                               | Familiar to many developers         | Ambiguous parsing; significant indentation-based errors in practice                                      | TOML is unambiguous and already used in this ecosystem               |
| JSON config                               | Machine-writable                    | No comments; annoying to hand-edit; trailing-comma errors                                                | TOML is better for human-edited config                               |
| Store vectors in NDJSON (committed)       | Single source of truth for all data | 15MB+ of non-diffable binary per 10K-entity KG; breaks git diff/merge guarantees; recomputable from text | ADR-048's diff/merge value requires human-readable NDJSON            |
| Dedicated vector storage file (committed) | Separates vectors from entity data  | Same merge problem as vectors in NDJSON; still grows quadratically with entity count                     | Recomputable state should not be committed                           |
| Manual embed only (no auto-embed)         | Explicit control                    | Silent quality degradation when users forget; no visible error                                           | Auto-embed on true prevents failure mode at negligible cost          |
| Embed on every verb write (real-time)     | Vectors always current              | Embed latency per create/update blocks interactive use                                                   | Batch on commit/sync is the right cadence for a git-workflow tool    |

## Consequences

### Positive

- Search quality is reliable: every collaborator who uses `khive kg sync` or `khive kg commit`
  has current vectors without any manual intervention.
- The embedding model is recorded in the repo alongside the KG data. Changing the model
  produces a reviewable one-line diff in `.khive/config.toml`.
- Device preferences stay local: no `device = "metal"` lines appear in committed files or PRs.
- `khive kg init` produces a minimal, well-commented `.khive/config.toml` that makes project
  defaults explicit and reviewable.
- `khive kg embed --dry-run` gives contributors visibility into which entities lack vectors
  before committing.

### Negative

- `khive kg commit` and `khive kg sync` now have an optional embed step that adds latency.
  For large KGs on slow hardware, this may be noticeable. Mitigation: `auto_embed = false`
  moves embedding to an explicit `khive kg embed` call that can be scheduled separately.
- Adding `~/.khive/config.toml` introduces user-level config that must be documented and
  supported. Misconfiguration (pointing to a non-existent model) produces a runtime error
  from lattice-embed rather than a config validation error. Mitigation: the CLI validates
  field types and reports file and line number on parse errors; model availability errors
  from lattice-embed are propagated with their full message.
- Changing `embed.model` requires re-embedding all entities (potentially slow for large KGs)
  and a follow-up commit of the config change. The workflow is documented but adds ceremony
  to model upgrades.

### Neutral

- The NDJSON files and their git history are unchanged. This ADR adds no new committed
  artifacts beyond `.khive/config.toml`.
- `working.db` already uses the sqlite-vec extension (ADR-009). The `entities_vec` virtual
  table is an extension of the schema already in ADR-052; this ADR specifies when it is
  populated, not how it is structured.
- Projects that do not use `khive search` (vector component) can set `auto_embed = false`
  and ignore the embed subsystem entirely. The config and pipeline changes are no-ops when
  `auto_embed = false` and `khive kg embed` is never called.

## Implementation

### Config loader (Deno/TypeScript)

A new `lib/config.ts` module handles the two-level merge:

```typescript
// cli/lib/config.ts

export interface EmbedConfig {
  model: string;
  dimensions: number;
  auto_embed: boolean;
  batch_size: number;
  device: string; // from global config only
  fields: { include: string[] };
}

export interface SchemaConfig {
  strict: boolean;
}

export interface KhiveConfig {
  embed: EmbedConfig;
  schema: SchemaConfig;
  auth: { api_url: string };
}

const DEFAULTS: KhiveConfig = {
  embed: {
    model: "mE5-small",
    dimensions: 384,
    auto_embed: true,
    batch_size: 64,
    device: "cpu",
    fields: { include: ["name", "description"] },
  },
  schema: { strict: true },
  auth: { api_url: "https://api.khive.ai" },
};

export async function loadConfig(projectRoot: string): Promise<KhiveConfig> {
  const global = await readToml(`${Deno.env.get("HOME")}/.khive/config.toml`);
  const project = await readToml(`${projectRoot}/.khive/config.toml`);
  // Project overrides global; global overrides defaults
  return deepMerge(deepMerge(DEFAULTS, global), project);
}
```

### Embedding integration in commit and sync

```typescript
// cli/commands/kg/commit.ts (extension of ADR-051 commit.ts)

import { loadConfig } from "../../lib/config.ts";
import { embedMissing } from "../../lib/embed.ts";

async function kgCommit(opts: CommitOptions): Promise<void> {
  const config = await loadConfig(opts.projectRoot);

  if (config.embed.auto_embed) {
    await embedMissing(opts.dbPath, config.embed);
  }
  // ... existing export → validate → git-add → git-commit pipeline
}
```

```typescript
// cli/lib/embed.ts

export async function embedMissing(
  dbPath: string,
  embedConfig: EmbedConfig,
): Promise<EmbedSummary> {
  // 1. Query working.db for entities with no vector at current model
  // 2. Concatenate embed.fields.include field values per entity
  // 3. Call lattice-embed in batches of embed.batch_size
  // 4. Write vectors to entities_vec virtual table
  // Returns { total: number, embedded: number, model: string }
}
```

### New CLI commands

```
cli/
  commands/kg/
    commit.ts    — extended: embed_missing before export (§5)
    sync.ts      — extended: embed_missing after rebuild (§5)
    embed.ts     — new: explicit embed command (§5)
  lib/
    config.ts    — new: two-level TOML merge (§1)
    embed.ts     — new: embed_missing subroutine (§5)
```

### Phasing

| Phase | Scope                                                                          | Target |
| ----- | ------------------------------------------------------------------------------ | ------ |
| E1    | `lib/config.ts` — TOML loader, two-level merge, validation                     | v0.5   |
| E2    | `khive kg init` — write default `.khive/config.toml`                           | v0.5   |
| E3    | `lib/embed.ts` — `embed_missing` subroutine; embed step in `commit` and `sync` | v0.5   |
| E4    | `khive kg embed` command — `--all`, `--ids`, `--dry-run` flags                 | v0.5   |
| E5    | Config validation error messages with file + line                              | v0.5   |

E1 and E2 are independently shippable and deliver the config model without any embedding
changes. E3 and E4 deliver the automatic embedding pipeline. E5 improves error messaging.

## References

- [ADR-048](ADR-048-git-native-kg-versioning.md) — NDJSON format, `khive kg init`, CLI commands
- [ADR-051](ADR-051-cli-auth-and-kg-git-workflow.md) — `khive kg commit` and `khive kg sync`
  pipelines extended in §5
- [ADR-052](ADR-052-kg-storage-model.md) — `working.db` schema; `.state/` layout; sqlite-vec
- [ADR-012](ADR-012-retrieval-architecture.md) — lattice-embed integration for local inference
- [ADR-056](ADR-056-kg-validation-pipelines.md) — validation pipelines that can consume config
  settings in future rules
- lattice-embed crate: local embedding model inference used by `embed_missing`
