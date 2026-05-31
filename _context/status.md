khive stats: entities=2478 | edges=7542 | notes=11623

issue# | root cause file:line | fix applied | test name+result | RESOLVED/SKIPPED
--- | --- | --- | --- | ---
#518 | `crates/khive-pack-kg/src/handlers.rs:363` | Added `tags: Option<Vec<String>>` to `SearchParams`; `tags_match_any` helper (OR, case-insensitive); entity_meta extended to `(kind, props, tags)`; post-filter uses `is_none_or` for both; 4x over-fetch when tag_filter active; `ParamDef` in lib.rs | `search_tags_params_accepts_tags` PASS (tester-run); `search_params_tags_absent_is_none` PASS (tester-run); `tags_match_any_or_semantics` PASS (tester-run); `search_tags_filter_restricts_results_or_semantics` PASS (tester-run, OR semantics confirmed: python+ml returned, rust-only excluded); cargo test --workspace RC=0 | RESOLVED (commit bb239e0) — TESTER VALIDATED 2026-05-31
#533 | `crates/khive-pack-knowledge/src/knowledge/vamana.rs:460` — `ensure_ann` returns when any `AnnBridge` is loaded; no namespace/model/fingerprint/generation metadata stored; `kkernel` cannot clear in-process `SharedAnn` | SKIPPED — cross-crate: requires durable corpus generation/hash sidecar in khive-db, `AnnBridge` metadata, fast-path revalidation, and `kkernel` pre/post reindex invalidation across 4 crates | future: `stale_live_bridge_rebuilds_after_reindex`; `same_count_reembed_changes_generation_and_rebuilds_ann` | SKIPPED (carry proposal — see fix_brief.md §#533) — TESTER CONFIRMED SKIP DOCUMENTED 2026-05-31
#551 | `crates/khive-pack-kg/src/lib.rs:793` — `KgPack` had no `warm()` override; ADR-049 daemon warm fanout calls pack warm hooks but KG skipped | Added `KgPack::warm()` — best-effort `tokio::spawn` sentinel embed; errors logged at debug; tokio added to `[dependencies]` | `warm()` present at lib.rs:816 (tester-verified); infallible design (Err logged at debug, no panic); cargo test --workspace RC=0; latency measurement not feasible in unit tests (requires live daemon) | RESOLVED (commit bb239e0) — TESTER VALIDATED 2026-05-31

## khive Usage Report (implementer additions)

Verbs called by implementer:
- `memory.recall(query="SearchParams entity search tags filter OR semantics...")` → `d3a148c3` confirmed #518 absent; guided implementation.
- `memory.remember(content="#518 entity tags filter...")` → `069d7725` recorded root cause + fix + file:line per issue.

Memory ids created by implementer: `069d7725`

---

## khive Usage Report (analyst — prior ops)
- `stats`: succeeded; analyst self-check returned entities=2478, edges=7542, notes=11623.
- `knowledge.search`: succeeded with `rerank=true`; useful hits covered retrieval embedding interfaces, safe ANN reindexing, and filtered retrieval metadata design.
- `search`: succeeded for retrieval/search concepts; useful existing entities included `Hybrid Retrieval Pipeline`, `ADR-012`, `Automatic Embedding Pipeline`, and `khive-vamana`.
- `get`: fetched full UUID for `khive-vamana`.
- `brain.feedback`: marked `khive-vamana` useful for this analysis; event `a8df0a9a`.
- `memory.recall`: first analyst call failed because this khive version does not accept a `tags` argument on `memory.recall`; retry without `tags` succeeded.
- `knowledge.suggest`: returned no matching domains for the exact retrieval-fixes query; no domain compose was available.
- `create`: emitted observation `8922199e` marking the analyst artifact ready.

Recalls that mattered:
- `d3a148c3`: #518 absent in current worktree; implement tags post-filter and tests.
- `994cf4a9`: #533 must be `SKIP-with-proposal`; contained fast-path check is insufficient.
- `742a27a3`: Vamana persistence/fingerprint sidecar guidance.
- `270ae811`: stale ANN snapshot defense via corpus fingerprint and active reindex invalidation.
- `7ebbf18b`: stale embedding/model hash caveat for daemon re-embed and warm/start paths.
- `669bef4e`: prior issue-sweep fix brief convention for surgical status rows.

Records created:
- `memory.remember`: `384727d5` records the fixed-batch triage hypotheses, issue sources, and artifact paths.
- `memory.remember`: `e6b51bbf` records the analyst consolidation brief and RESOLVE/SKIP recommendations; see `fix_brief.md` in `shows/khive-issue-sweep/retrieval-fixes/an/`.
- `create`: `8922199e` records the analysis-ready observation for team coordination.
- `brain.feedback`: `a8df0a9a` marks `khive-vamana` (`3a5e0fb3-120f-467a-a500-bf37f23da617`) useful.

Issue inventory:
- `_context/issues.json` contains the raw requested issue-list output. The exact `--label bug,enhancement` query returned `[]`; the fixed batch is therefore taken from the explicit issue list in the task, not from that inventory.
