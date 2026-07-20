# khive-pack-memory

The memory verb pack for khive. Provides `memory.remember` and `memory.recall`
with decay-aware hybrid ranking over a dedicated `memory` note kind, plus
feedback and curation verbs.

## Verbs

| Verb              | What it does                                                           |
| ----------------- | ---------------------------------------------------------------------- |
| `memory.remember` | Create a memory note with salience and decay                           |
| `memory.recall`   | Recall memories via decay-aware hybrid (FTS + vector) ranking          |
| `memory.feedback` | Emit explicit feedback on a recalled entity; updates recall posteriors |
| `memory.prune`    | Soft-delete memories below a salience floor and/or past `expires_at`   |
| `memory.vacuum`   | Run SQLite `VACUUM` to reclaim space freed by soft-deleted rows        |

These five are `Visibility::Verb` (MCP-callable). `MemoryPack` also declares
five `Visibility::Subhandler` entries (`memory.recall_embed`,
`recall_candidates`, `recall_fuse`, `recall_rerank`, `recall_score`) that
expose the recall pipeline's intermediate stages for introspection — they are
not dispatched over the MCP wire.

## Decay-aware ranking

Every memory carries a `salience` (0.0-1.0) and a `decay_factor` (>= 0, higher
decays faster). Defaults are type-differentiated
(`memory.remember`'s `memory_type` parameter, `src/pack.rs`):

| `memory_type`        | default `salience` | default `decay_factor` | approx. half-life |
| -------------------- | ------------------ | ---------------------- | ----------------- |
| `episodic` (default) | 0.3                | 0.02                   | ~35 days          |
| `semantic`           | 0.5                | 0.005                  | ~139 days         |

Explicit caller-supplied values always override these defaults. `memory.recall`
fuses lexical and vector retrieval (`khive-fusion`, `khive-retrieval`,
`khive-vamana`) and folds decay into the composite score, which is always
normalized to `[0, 1]`; `min_score` / `score_floor` filter below a threshold.
Recall results also route feedback signals into per-namespace Beta-posterior
state (`khive-brain-core`'s `BalancedRecallState`) that tunes future ranking.

## Usage

`MemoryPack` requires the `kg` pack (`REQUIRES = ["kg"]`) and a registered
embedding provider for vector recall (`KhiveRuntime::register_embedder`):

```rust
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

let mut builder = VerbRegistryBuilder::new();
builder.register(KgPack::new(runtime.clone()));
builder.register(MemoryPack::new(runtime));
let registry = builder.build()?;

registry
    .dispatch(
        "memory.remember",
        json!({"content": "Vamana is sublinear at low intrinsic dimension", "memory_type": "semantic"}),
    )
    .await?;

let hits = registry
    .dispatch("memory.recall", json!({"query": "Vamana intrinsic dimension", "limit": 10}))
    .await?;
```

Over MCP: `request(ops="memory.recall(query=\"Vamana intrinsic dimension\", limit=10)")`.

## Where this sits

`khive-pack-memory` sits alongside `khive-pack-gtd`, `khive-pack-comm`, and
`khive-pack-schedule` in the pack layer, depending on `khive-pack-kg` for the
note substrate and on `khive-fusion` / `khive-retrieval` / `khive-vamana` /
`khive-brain-core` for hybrid scoring, ANN recall, and decay-posterior state.
It registers into `khive-runtime`'s `VerbRegistry`, consumed by `khive-mcp`.
Governing ADR:
[ADR-021](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-021-memory-pack.md) (memory pack),
built on [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md) (pack standard)
and [ADR-014](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-014-curation-operations.md) (curation operations, for `prune`/`vacuum`).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
