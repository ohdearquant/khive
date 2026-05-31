Verdict: REQUEST CHANGES
Findings: 0 Critical, 3 Major, 0 Minor, 0 Suggestions

## Findings

### [Major] `rerank=false` is not a no-embedding opt-out once ANN is warm

Evidence: `crates/khive-pack-knowledge/src/knowledge/mod.rs:1075` defaults the new `requested_rerank` flag, and `crates/khive-pack-knowledge/src/knowledge/mod.rs:1125` uses it only to gate `rerank_with_embeddings`. The ANN path above it still runs whenever `ann.index` is present: `crates/khive-pack-knowledge/src/knowledge/mod.rs:1111` embeds the query and fuses Vamana hits via RRF without checking `requested_rerank`. The new benchmark describes `rerank=false` as "pure TF-IDF, no embedding" at `crates/khive-pack-knowledge/tests/bench.rs:6`, but that is only true when the ANN bridge is absent; `KnowledgePack::warm` can preload the ANN bridge via `warm_known_snapshots` at `crates/khive-pack-knowledge/src/lib.rs:581`.

Why this matters: Users and the benchmark now treat `rerank=false` as the latency/control opt-out for embeddings. In a daemon that has restored a Vamana snapshot, `rerank=false` can still pay query embedding cost and alter ranking through ANN fusion, so the opt-out contract and the warm latency baseline are misleading.

Suggested fix: Either gate ANN fusion on `requested_rerank` as well, or split the controls explicitly (for example `semantic`/`ann` vs `rerank`) and update the benchmark/docs to stop calling `rerank=false` pure TF-IDF. Add a regression with a preloaded ANN bridge proving the intended behavior.

### [Major] FTS5 operator hardening still misses column-filter syntax characters

Evidence: `crates/khive-db/src/stores/text.rs:254` filters special characters before passing Plain-mode user text to MATCH, but the blocked set at `crates/khive-db/src/stores/text.rs:257` omits `{`, `}`, `[`, and `]`. The sanitized query is then sent directly as the MATCH expression at `crates/khive-db/src/stores/text.rs:571`. The new KG matrix at `crates/khive-pack-kg/tests/integration.rs:1101` covers quotes, boolean operators, NEAR, wildcard, colon, caret, parentheses, hyphen, and apostrophe, but not FTS5 column-filter braces/brackets. A direct FTS5 parser check shows these are still dangerous: `MATCH '{tenant isolation}'` returns `no such column: tenant`, `MATCH 'tenant } isolation'` returns `fts5: syntax error near "}"`, and `MATCH 'tenant [ isolation'` returns `fts5: syntax error near "["`.

Why this matters: The PR claims full operator-class coverage across db/KG surfaces, but user queries containing FTS5 column-filter syntax can still surface storage errors instead of safe empty/normal results.

Suggested fix: Treat `{`, `}`, `[`, and `]` as separators or stripped operator characters in `sanitize_fts5_query`, then add db and KG regression cases for balanced/unbalanced braces and brackets.

### [Major] `KnowledgePack::warm` does not guarantee the embedder is warm before the hot path

Evidence: `KnowledgePack::warm` awaits Vamana snapshot warmup at `crates/khive-pack-knowledge/src/lib.rs:581`, but the new embedder warmup at `crates/khive-pack-knowledge/src/lib.rs:585` detaches a `tokio::spawn` and immediately returns. The daemon already schedules pack warmup in a background task at `crates/khive-runtime/src/daemon.rs:221`, so the embedder warm is effectively double-detached from request readiness. The new benchmark also does not exercise `KnowledgePack::warm`: it constructs a registry at `crates/khive-pack-knowledge/tests/bench.rs:67`, then measures the first default search as cold at `crates/khive-pack-knowledge/tests/bench.rs:93`; its JSON note at `crates/khive-pack-knowledge/tests/bench.rs:167` defines warm as "after first reranked query preloads embedding model".

Why this matters: The PR description says cold first-query cost is eliminated from the hot path, but this implementation only starts a best-effort background embed. A request arriving before that spawned task completes can still pay the model-load cost, and the benchmark does not verify the `warm()` path that the PR is changing.

Suggested fix: If the intended contract is warm-before-ready, await `runtime.embed("__khive_knowledge_warm__")` inside `KnowledgePack::warm` and benchmark a first query after `registry.call_warm_all().await` or server `warm_all().await`. If startup must remain non-blocking, update the claim and benchmark to reflect best-effort background warming, and expose/measure warm completion separately.

## Looks Right

- The RRF normalization formula itself is the theoretical maximum form requested: `source_count / (k + 1)` at `crates/khive-pack-knowledge/src/knowledge/mod.rs:1423`, matching the 1-indexed `1 / (k + rank)` implementation in `crates/khive-score/src/ops.rs:102`.
- The default rerank flip is guarded for no-embedder runtimes at `crates/khive-pack-knowledge/src/knowledge/mod.rs:1075`, and `KhiveRuntime::memory()` still has `embedding_model: None` at `crates/khive-runtime/src/runtime.rs:337`.
- Knowledge-pack phrase quoting looks safer than the generic TextSearch path: `quote_fts5_phrase` wraps the full raw query and doubles embedded quotes at `crates/khive-pack-knowledge/src/knowledge/mod.rs:1886`.

## Commands Run

- `git status --short --branch`: clean PR worktree on `show/khive-issue-sweep/knowledge-search`.
- GitHub PR metadata: local `HEAD` `340b0f7243b36f4c8b7178e0ff819d97dfca4e0c` matches PR #601 head; base is `main` `eefe5568978774f1d29b03506c9be8e9fa987c52`.
- `RUSTC_WRAPPER= cargo test -p khive-db test_sanitize_fts5_query -- --nocapture`: passed.
- `RUSTC_WRAPPER= cargo test -p khive-pack-knowledge --test fixes fts_operator_matrix_does_not_crash -- --nocapture`: passed.
- `RUSTC_WRAPPER= cargo test -p khive-pack-knowledge --test fixes search_defaults_to_embedding_rerank_when_embedder_configured -- --nocapture`: passed.
- `RUSTC_WRAPPER= cargo test -p khive-pack-knowledge --test bench bench_infrastructure_smoke_test -- --nocapture`: passed.
- `RUSTC_WRAPPER= cargo test -p khive-pack-kg search_operator_matrix_does_not_crash -- --nocapture`: passed.
- `sqlite3 :memory: ... MATCH '{tenant isolation}' / 'tenant } isolation' / 'tenant [ isolation'`: reproduced FTS5 parser errors for still-unsanitized operator syntax.

## What I Did Not Check

- I did not run `cargo test --workspace` or clippy due the 20-minute review deadline.
- I did not run the ignored latency benchmark because it is manual, environment-sensitive, and does not currently exercise `KnowledgePack::warm`.

## Re-Review Guidance

Re-review should be narrow: rerun the three requested behavior areas after fixes for `rerank=false` semantics, FTS5 brace/bracket sanitization, and the warm-start benchmark/contract.

Domain utility: SKIPPED - the khive domain-suggestion MCP call was cancelled before returning, so this review relied on repository ADRs, code, and targeted tests.
