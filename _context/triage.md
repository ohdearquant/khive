# Retrieval Fix Batch Triage

Preflight source set:
- khive `stats()` on 2026-05-31: entities=2475, edges=7542, notes=11602.
- `gh issue view 518`, `gh issue view 533`, `gh issue view 551` run on 2026-05-31 from `/Users/lion/khive-work/worktrees/khive-issue-sweep-retrieval-fixes`.
- `gh issue list --label bug,enhancement --state open --limit 20 --json number,title,body,labels,assignees` saved raw to `_context/issues.json`; exact query returned `[]`.

## Fixed Issues

| issue# | title | initial root-cause hypothesis | VERIFY/ASSESS directive | confidence |
| --- | --- | --- | --- | --- |
| #518 | feat(search): tags filter on entity search | The issue body says `SearchParams.tags` and post-filtering are already implemented in `crates/khive-pack-kg/src/handlers.rs`; likely remaining work is regression coverage and help/doc verification, not a reimplementation. | VERIFY-FIRST: grep `SearchParams` for `tags`; if present and working, add/confirm a test proving OR-semantics tag-filtered entity search returns only entities with matching tags, plus help/doc text. | C:0.8, source: `gh issue view 518` (2026-05-31), context: issue body states "Already implemented and compiled." |
| #533 | Vamana live ANN cache coherence after reindex (generation/fingerprint revalidation) | `invalidate_vamana_snapshots` appears to invalidate persisted snapshots only, while a live `SharedAnn` may continue serving a cached `AnnBridge` unless `ensure_ann` revalidates namespace/model/fingerprint/generation metadata. | ASSESS-SCOPE: inspect `crates/khive-db/src/stores/vectors.rs` for `ensure_ann`, `invalidate_vamana_snapshots`, and whether `AnnBridge` carries generation/fingerprint metadata; resolve only if contained, otherwise SKIP with a proposal. | C:0.8, source: `gh issue view 533` (2026-05-31), context: issue body identifies the fast-path cache-coherence failure mode. |
| #551 | perf(runtime): first search() pays 50-58s embedding-model lazy-load | `NativeEmbeddingService` likely lazy-loads model weights on the first embedding call; current risk is whether ADR-049/#598 daemon warm-start already covers daemon starts, leaving only cold MCP-only startup unwarmed. | CHECK DAEMON WARM PATH FIRST: grep `NativeEmbeddingService` / `EmbeddingService` in `crates/khive-runtime` and `crates/khive-mcp/src/server.rs`; add eager warm only where the existing warm path does not cover the cold-start mode. | C:0.8, source: `gh issue view 551` (2026-05-31), context: issue body reports measured first-call latency and points to lazy model loading. |

## khive Orient Notes

Recalls that mattered:
- `742a27a3`: Vamana persistence should use `{ns}::vamana::{model}` with typed snapshots and a write-maintained corpus fingerprint sidecar.
- `270ae811`: ANN snapshot staleness is a known W2/W3-class bug; defense is self-validating corpus fingerprint on load plus active invalidation on reindex.
- `7ebbf18b`: Daemon auto re-embed work previously required clearing persisted embeddings and stale model hash, reinforcing that warm/start behavior may involve more than changing config.

Knowledge hits that mattered:
- `Embedding Preprocessing Contracts for ANN Indices and Safe Reindexing` (score 0.833) supports reindex/fingerprint consistency checks.
- `Filtered Vector Search Patterns` (score 0.793) supports the #518 post-filter/over-fetch framing.
- `Semantic Search with Embeddings` and `Hybrid Search` were background only, not decisive.

Gaps for explorers:
- #518 still needs code verification of `SearchParams.tags`, the actual post-filter site, and the help descriptor.
- #533 still needs code verification of whether live `AnnBridge` metadata exists; without that, scope cannot be called contained.
- #551 still needs code verification of ADR-049/#598 warm path coverage and where MCP-only cold startup enters.

Domain utility: SKIPPED - this was an internal codebase preflight; khive memory and knowledge search supplied the relevant local context.
