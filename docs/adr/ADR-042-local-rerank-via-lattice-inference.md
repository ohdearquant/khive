# ADR-042: Composable Local Reranking

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-011](./ADR-011-embedding-and-inference.md) (Embedding and Inference Architecture)
- [ADR-031](./ADR-031-multi-engine-retrieval.md) (Multi-Engine Retrieval)

---

## Context

Knowledge-graph search combines lexical and vector candidates before returning a ranked result.
The public runtime already supports a deterministic local scoring stage after fusion. A model-based
cross-encoder is a different capability: it must hydrate candidate content, score each
query-candidate pair, and define explicit latency and failure behavior.

The public distribution does not ship a model-based reranker. This ADR therefore records the
shipped local behavior and the extension boundary without prescribing an unpublished model
runtime or configuration surface.

## Decision

### 1. The shipped rerank is deterministic and local

Note search follows this order:

1. obtain lexical candidates;
2. obtain vector candidates when an embedding model is configured;
3. combine the ranked inputs with reciprocal-rank fusion;
4. hydrate the candidate notes and discard records that are deleted or fail caller filters;
5. multiply each surviving fused score by `0.5 + 0.5 * salience`;
6. sort deterministically and truncate to the requested limit.

The weighting is part of the note-search contract. It requires no network call and no additional
model state. Because `Note.salience` is a public substrate field, this rule remains available to
every caller of the KG pack.

### 2. Generic rerank implementations use the retrieval trait

The retrieval crate exposes a backend-neutral seam:

```rust
#[async_trait]
pub trait Reranker<Id: Send + Sync + 'static>: Send + Sync {
    async fn rerank(
        &self,
        query: &str,
        results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Result<Vec<(Id, DeterministicScore)>>;
}
```

An implementation receives already ranked identifiers and scores. It may reorder or rescore those
candidates, but it must not expand the candidate set, bypass namespace filtering, or return more
than `top_k` results.

### 3. Model-based reranking is deferred

This ADR does not define a model registry entry, runtime model identifier, adapter mechanism, or
model-specific event payload. Adding a native cross-encoder requires a later ADR that specifies:

- the public model-loading and configuration contract;
- candidate hydration and maximum input lengths;
- timeout, cancellation, and bounded-concurrency behavior;
- deterministic fallback when scoring is unavailable;
- score normalization and tie-breaking;
- public telemetry that does not expose model internals.

Until that contract is accepted and implemented, callers receive the shipped deterministic
pipeline described above.

### 4. Failure behavior

Failures in lexical search, vector search, or record hydration propagate through the normal search
error contract. The deterministic salience calculation is total for valid stored notes. A future
optional reranker must preserve the pre-rerank ordering when it declines a request or is unavailable;
it must never return a partial reordering as a successful response.

## Invariants

1. Namespace and record-visibility checks complete before optional reranking.
2. Reranking can only reorder or rescore the candidate set supplied to it.
3. Equal final scores use the existing deterministic identifier tie-break.
4. The final result never exceeds the caller's requested limit.
5. Embedding generation and stored-vector identity are unaffected by reranking.
6. No unpublished model or adapter configuration is part of the public wire contract.

## Consequences

### Positive

- The public ADR matches shipped behavior.
- Search remains deterministic and operational without a separate inference service.
- The `Reranker` trait leaves room for independently implemented local scorers.
- Future model-based work has an explicit security and reliability checklist.

### Tradeoffs

- The shipped score does not model query-candidate interaction beyond fused retrieval signals.
- Adding a cross-encoder remains a separate implementation and specification task.
- Implementations that hydrate text for reranking must account for additional storage reads.

## Testing requirements

- Fused note scores are multiplied by `0.5 + 0.5 * salience`.
- Filtering occurs before truncation so eligible lower-ranked candidates are not lost.
- Equal scores produce stable identifier ordering.
- Searches without a configured embedding model still execute the lexical path.
- A custom `Reranker` cannot increase the candidate count or exceed `top_k`.

## References

- [ADR-011](./ADR-011-embedding-and-inference.md): embedding and inference boundary
- [ADR-012](./ADR-012-retrieval-composition.md): retrieval composition
- [ADR-031](./ADR-031-multi-engine-retrieval.md): multi-engine fusion
