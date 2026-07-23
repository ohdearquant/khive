# ADR-043: Embedding Model Migration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-011](./ADR-011-embedding-and-inference.md) (Embedding and Inference Architecture)
- [ADR-022](./ADR-022-events-query-surface.md) (Events Query Surface)
- [ADR-031](./ADR-031-multi-engine-retrieval.md) (Multi-Engine Retrieval)
- [ADR-044](./ADR-044-vector-store-extensions.md) (Vector Store Extensions)

---

## Context

A stored vector is meaningful only in the embedding space that produced it. Changing the active
model without tracking vector provenance can mix incompatible spaces and silently degrade vector
search. Multi-engine configurations add a related requirement: every write and query must resolve
an explicit model when more than one model is registered.

The runtime and database therefore need a durable model registry, per-vector model identity, and a
bounded reindex workflow. These are local storage and runtime concerns. They do not require a
remote model-management service.

## Decision

### 1. Register embedding models durably

The database maintains `_embedding_models`, keyed by engine name and model identity. Each row
records enough information to validate compatibility, including the model name, revision,
dimension, distance metric, data type, normalization, status, and timestamps.

Registration is idempotent when the compatibility fields match. Reusing an existing identity with
different compatibility fields is an error. Human-readable aliases are configuration conveniences;
the stored canonical model name is authoritative.

### 2. Tag every vector with its model

Vector rows include a nonempty `embedding_model` value. The runtime selects a vector store by that
value, and vector search never compares a query vector against rows tagged for another model.

For databases created before model tagging, migration may assign the configured model only when a
single model is unambiguous. If no safe assignment exists, the rows remain unavailable to vector
search until they are reindexed. The migration must not guess between multiple configured models.

### 3. Resolve model selection before writes

Create and update operations resolve the requested model before writing the record, full-text
index, or vector. An unknown model fails the operation before any of those writes occur.

When no model is requested, the configured default is used. When no default exists, operations that
require embedding return an unconfigured-model error. Text-only record creation may proceed when
the calling operation explicitly permits vector indexing to be absent.

### 4. Reindex through a resumable worker

Model replacement uses a bounded worker rather than an unbounded transaction:

1. register the target model;
2. mark the source-to-target migration as pending;
3. read source records in stable identifier order;
4. compute target embeddings in bounded batches;
5. write each target vector with the target model tag;
6. persist the cursor and counters after every committed batch;
7. mark the target model active only after validation succeeds;
8. retire old vectors through the normal vector-store deletion path when policy permits.

The worker is restartable. A repeated batch must converge on the same vector rows rather than create
duplicates. Record content remains the source of truth throughout the migration.

### 5. Keep model spaces separate during migration

Migration does not require a mixed-space search. The source and target vector indexes remain
separate. A caller selects one registered model for a vector query, while lexical search continues
to operate independently. If an application wants to query both model spaces, it performs two
searches and combines ranked results through the fusion contract in ADR-031.

### 6. Emit lifecycle events without embedding content

Migration events report identifiers, source and target model names, state, cursor, counts, and an
error category when applicable. Events must not contain source text or vector values. Event
publication follows the existing events surface in ADR-022 and does not alter migration state.

## State model

The durable migration state is one of:

| State        | Meaning                                           |
| ------------ | ------------------------------------------------- |
| `pending`    | Registered but not yet started                    |
| `running`    | Batches are being processed                       |
| `paused`     | Progress is retained and no batch is active       |
| `validating` | Target coverage and dimensions are being checked  |
| `completed`  | The target is valid and available for selection   |
| `failed`     | Processing stopped with a durable error category  |
| `cancelled`  | Processing stopped by an explicit operator action |

Only the worker may advance the cursor. State transitions use compare-and-set updates so concurrent
workers cannot claim the same migration.

## Validation

Before completion, the worker verifies:

- every eligible source record has exactly one target vector;
- target vector dimensions match the registered model;
- no target row carries the source model tag;
- deleted records are not reintroduced;
- a sampled query returns only vectors from the selected target space.

Distribution-drift statistics may be reported by independent tooling, but they are diagnostic. They
do not replace the structural checks above and are not part of this ADR's public API.

## Failure handling

- Batch failures retain the last committed cursor and move the migration to `failed`.
- Retrying from `failed` requires an explicit transition back to `pending` or `running`.
- Cancellation does not delete completed target batches; cleanup is a separate idempotent action.
- A missing source record is counted as skipped if it was deleted after the migration began.
- Model execution errors are recorded by category without persisting record content.

## Consequences

### Positive

- Incompatible embedding spaces cannot be mixed silently.
- Multi-engine reads and writes use an explicit, auditable model identity.
- Large migrations are restartable and bounded in transaction size.
- Lexical retrieval remains available while vectors are rebuilt.

### Tradeoffs

- A migration temporarily stores vectors for both source and target models.
- Model changes require explicit operator action and validation.
- Applications that query multiple spaces must fuse their result lists explicitly.

## Testing requirements

- Conflicting registrations of the same model identity fail.
- An unknown requested model fails before record, text-index, or vector writes.
- Querying model A never returns a vector tagged for model B.
- Worker restart resumes after the last committed cursor without duplicates.
- Completion is rejected when coverage or dimension validation fails.
- Migration events contain no source content or raw vectors.

## References

- [ADR-011](./ADR-011-embedding-and-inference.md): embedding execution contract
- [ADR-022](./ADR-022-events-query-surface.md): event storage and filtering
- [ADR-031](./ADR-031-multi-engine-retrieval.md): cross-engine rank fusion
- [ADR-044](./ADR-044-vector-store-extensions.md): vector maintenance operations
