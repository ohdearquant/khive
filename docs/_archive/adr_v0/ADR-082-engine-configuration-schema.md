# ADR-082: Engine Configuration Schema — `[[engines]]` TOML and Vector Table Naming

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-057 (CLI Configuration and
Embedding — two-level TOML override)\
**Partially supersedes**: ADR-057 §"Why project config beats global config" — extends the
single-`embed.model` invariant to a multi-engine `[[engines]]` array with the same project-
override semantics\
**Part of**: ADR-078 (Multi-Engine Embedding umbrella)

## Context

ADR-081 defines the `Embedder` trait and `EmbedderRegistry`. ADR-082 specifies the **user-
facing configuration surface** — the TOML schema operators write to declare N peer
embedding engines, plus the vector table naming convention that maps engine identifiers to
storage tables.

khive-internal had this schema as `deploy/engine.toml`'s `[[embedding_models]]` array. The
open-core port collapsed to a single-model invariant; this ADR restores the multi-engine
schema, aligned with the rest of khive's TOML layering (ADR-057's user + project two-level
config).

## Decision

### 1. Configuration schema — `[[engines]]` in `khive.toml`

Engines are declared as a TOML array. The same array appears in either user-level
(`~/.khive/khive.toml`) or project-level (`.khive/khive.toml`) config; project overrides
user.

```toml
# All engines are peers. Every write embeds with every configured engine.
# Every query embeds with every configured engine and searches every index in parallel.
# Per-engine ranked results fuse via weighted RRF using the `weight` field below.

[[engines]]
name = "bge-small-en-v1.5"      # canonical Embedder model_id; snake_case_for_tables
dim = 384                        # output dim — must match embedder's dim() or MRL truncation
weight = 1.0                     # RRF fusion weight (per ADR-084)
noise_floor = 0.30               # cosine score below this is discarded
max_similarity = 0.75            # cosine score cap for normalization
threshold = 0.25                 # per-engine minimum to enter fusion
# device = "metal"               # optional, lattice-embed-specific (machine-local)

[[engines]]
name = "multilingual-e5-small"
dim = 384
weight = 0.8
noise_floor = 0.15
max_similarity = 0.65
threshold = 0.30

[[engines]]
name = "qwen3-embedding-0.6b"
dim = 1024
weight = 1.2
noise_floor = 0.10
max_similarity = 0.70
threshold = 0.20
output_dim = 512                 # optional MRL truncation; only for supports_output_dim() models
```

The corresponding Rust type (per ADR-081 §`EmbedderRegistry`):

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub name: String,                 // exactly the TOML `name` field
    pub dim: usize,
    pub weight: f32,
    pub noise_floor: f64,
    pub max_similarity: f64,
    pub threshold: f64,
    pub output_dim: Option<usize>,
}
```

### Two-level override semantics

User-level `~/.khive/khive.toml` declares default engines. Project-level
`.khive/khive.toml` MAY declare its own `[[engines]]` array. If project-level declares the
array, it **replaces** the user-level list entirely. There is no per-entry merge.

**Why replace, not merge**: project config locks the engine set for collaboration
consistency. ADR-040's model-as-invariant logic (a project's vectors are bound to a specific
model) extends here — collaborators sharing a project must agree on which engines to run;
allowing per-entry override would break that invariant silently.

Engine-level fields that are inherently machine-specific (e.g., `device = "metal"` for
Mac, `device = "cuda"` for Linux) are user-level only — they describe the local execution
environment, not the project's engine list. The project's `name` / `dim` / `weight` /
calibration fields are what the project commits to.

### Single-engine fallback

If neither user nor project declares `[[engines]]`, khive falls back to a built-in default:
one engine entry for the default embedding model (`bge-small-en-v1.5`, 384-dim, weight 1.0,
calibrated default normalization parameters). This is the migration shim for deployments that
predate ADR-082; new deployments should declare engines explicitly.

### Vector table naming

Vector tables are sharded by engine — one `vec_*` table per (model_id, output_dim) pair. The
naming convention:

- Base table: `vec_{snake_case(model_id)}`
  - `bge-small-en-v1.5` → `vec_bge_small_en_v1_5`
  - `multilingual-e5-small` → `vec_multilingual_e5_small`
  - `qwen3-embedding-0.6b` → `vec_qwen3_embedding_0_6b`
- MRL variants: `vec_{snake_case(model_id)}_dim_{N}`
  - `qwen3-embedding-4b` native (2560d) → `vec_qwen3_embedding_4b`
  - `qwen3-embedding-4b` MRL-truncated to 1024d → `vec_qwen3_embedding_4b_dim_1024`

The sanitization rule (`vec_model_key`): replace every non-alphanumeric character with `_`.
This is already implemented in `khive-runtime`'s `sanitize_key` helper; the rule moves to
`khive-embed` as it becomes the canonical engine-identity-to-storage-key bridge.

### Engine name vs. model_id

The TOML field `name` is the same string as `Embedder::model_id()`. They are not two
identifiers — they are one identifier with two contexts (operator-facing TOML vs.
implementation-facing trait method). The snake_case transformation happens at table-key
construction, not at config-load: `EngineConfig::name` keeps the hyphenated form;
`vec_model_key(name)` produces the table key.

This is an intentional design choice — operators write what they see in lattice-embed's
`EmbeddingModel` enum (`"bge-small-en-v1.5"`), and the storage layer handles the SQL-safe
transformation transparently.

## Layering

| Concern                                    | Crate                                        | Why                                             |
| ------------------------------------------ | -------------------------------------------- | ----------------------------------------------- |
| `EngineConfig` type + TOML deserialization | `khive-embed` (per ADR-081)                  | Co-located with `EmbedderRegistry`              |
| TOML loader (`load_engines(path)`)         | `khive-embed`                                | One place for the parse                         |
| User/project override resolution           | `khive-mcp` (boot) or `khive-config` interim | Boot-time wiring; reuses ADR-057's pattern      |
| `vec_model_key()` (snake_case bridge)      | `khive-embed` (canonical)                    | Replaces runtime's helper                       |
| `vec_*` tables (vec0 virtual tables)       | `khive-db` (already exists)                  | Storage-layer; one table per `(model_key, dim)` |

## Alternatives Considered

### A. One table, model_id-keyed rows

Single global vec0 table; embed rows tagged with `model_id`. Rejected: HNSW indexes are
dimension-fixed (a 384d node and a 1024d node cannot share a graph). Per-(model, dim) table
sharding exists for correctness, not optimization — breaks the INV-1 invariant from
`foundation/embed/DESIGN.md`.

### B. Engine config split into "identity" and "calibration" tables

`[[engines]]` for `name`/`dim`/`weight`; separate `[[calibration]]` for `noise_floor`/`max_
similarity`/`threshold`. Considered for clarity but rejected for v1 — calibration changes
more often than identity, but in practice operators tune them together. Future ADR may
split.

### C. Per-engine `write_eligible` flag (allowlist writes)

`[[engines]] write_eligible = false` would let an engine serve reads without storing every
write. Considered for storage-cost mitigation. Rejected v1 — defaults to "every engine
embeds every write" (khive-internal behavior). Future ADR if storage cost becomes a
constraint.

### D. Per-engine `device` field as project-level

Allowing `device = "metal"` in project config. Rejected — devices are operator-machine-
local; collaborators running on Linux + Mac shouldn't have to override a "metal"-pinned
project setting.

### E. Replace user-level on project-level partial merge

`[[engines]]` entries with matching `name` merge field-by-field; others ignored. Rejected:
silent merge surprises break the project-as-invariant principle.

## Consequences

### Positive

- Multi-engine restored — same schema as khive-internal's `deploy/engine.toml`
- Project consistency — collaborators share the same engine list per project
- Operator-tunable calibration — knob exposure matches what khive-internal had
- Vector tables naturally shard per engine; no schema redesign needed
- MRL variants (output_dim < native) get distinct tables; no graph corruption
- Single-engine fallback preserves backwards compat for deployments without `[[engines]]`

### Negative

- TOML schema burden — operators must learn the `[[engines]]` array shape
- Calibration parameters need empirical tuning per engine (noise_floor / max_similarity /
  threshold); defaults are inherited from khive-internal but per-corpus tuning may be
  needed
- Project replacement (not merge) means a project that wants to add ONE engine must
  re-declare the full list — verbose but explicit

### Neutral

- ADR-057's user-vs-project override pattern is reused — no new layering decision
- `vec_model_key()` lives in `khive-embed`; runtime's existing helper deprecates with no
  behavior change
- Single-engine deployments continue to work; legacy `vec_default` table can be renamed via
  one-time migration in Phase B of ADR-078 implementation

## Open Questions

1. **Calibration as separate "scoring profile" table.** Today `[[engines]]` mixes identity
   (name, dim) with calibration (noise_floor, max_similarity, threshold, weight). Calibration
   changes more often than identity. Future ADR could split them into two tables. v1: ships
   together per khive-internal's shape.
2. **Multi-engine write policy.** Default: every engine embeds every write. Storage cost is
   3× with 3 engines. A future `write_engines` allowlist could be added. v1: simple shape.
3. **Remote-API engine config.** A `[[engines]] provider = "openai"` shape needs distinct
   fields (`api_key_env`, `endpoint`, `timeout`). Not in v1; the `Embedder` trait supports
   it but the TOML schema is BGE/E5/Qwen3 (lattice) shaped for now. Future ADR for
   remote-API providers.

## References

- ADR-005 — Storage Capability Traits (`VectorStore` trait + `vec_model_key` pattern)
- ADR-040 — Embedding Model Migration (model-as-invariant logic; this ADR extends to the
  engine list)
- ADR-057 — CLI Configuration and Embedding (two-level TOML override pattern reused here)
- ADR-078 — Multi-engine embedding umbrella
- ADR-081 — `Embedder` trait and `EmbedderRegistry` (consumer of `EngineConfig`)
- ADR-083 — Runtime API change (uses `model_id` for table routing)
- ADR-084 — Pack multi-engine orchestration (uses `weight` for RRF fusion)
- khive-internal `deploy/engine.toml` — canonical schema being restored
- khive-internal `foundation/types/src/settings.rs:179-253` — `EmbeddingModelConfig` parser
- khive-internal `.khive/archive/engine_v1/src/config.rs:79-285` — `EmbedModelConfig` V1
  shape
