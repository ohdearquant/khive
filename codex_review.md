# Codex Review - impl-c06 Event Observable and Provenance Model

Verdict: REJECT

Scope reviewed: commit `20a8599` on `show/adr-001-015-alignment/impl-c06` against `show/adr-001-015-alignment/integration`, with the cluster-06 spec and accepted ADRs as the contract.

## Findings

### Critical - Migration versioning violates the accepted ADR-015 ledger

Evidence:
- `docs/adr/ADR-015-schema-migrations.md:37` assigns V5 to ADR-043 `embedding_pipeline_extensions`.
- `docs/adr/ADR-015-schema-migrations.md:38` assigns V6 to ADR-046 `event_sourced_proposals_index`.
- `docs/adr/ADR-015-schema-migrations.md:39` assigns V7 to ADR-041 `event_observations_and_session_id`.
- `docs/adr/ADR-015-schema-migrations.md:40` assigns V8 to ADR-022 `events_namespace_ts_id_idx`.
- `crates/khive-db/src/migrations.rs:184` labels event observability and provenance as V5.
- `crates/khive-db/src/migrations.rs:211` registers version 5 as `event_observability_provenance`.
- `crates/khive-db/src/migrations.rs:358` builds all event observability SQL from a V5 migration helper.
- `crates/khive-db/src/migrations.rs:407` through `crates/khive-db/src/migrations.rs:412` creates the event kind/session/proposal/provenance indexes under that same V5 helper.
- `crates/khive-db/src/migrations.rs:433` and `crates/khive-db/src/migrations.rs:443` assert latest migration version/count is 5.

Why this blocks: the diff steals V5 from ADR-043 and collapses ADR-046, ADR-041, and ADR-022 schema ownership into one version. That breaks the migration ledger contract, makes later cluster ordering unsafe, and can strand databases that already apply an ADR-043 V5 migration.

Fix: preserve or implement ADR-043 as V5, split event proposal/index/provenance/query-index work into the ADR-assigned V6, V7, and V8 migrations, and update migration tests to assert the accepted ledger names and latest version.

### High - `list(kind="event")` does not expose the required event filter contract

Evidence:
- `docs/adr/ADR-022-events-query-surface.md:88` through `docs/adr/ADR-022-events-query-surface.md:96` require event-list wire fields `kind`, `kinds`, `verb`, `verbs`, `outcome`, `actor`, `substrate`, `since`, and `until`.
- `docs/adr/ADR-022-events-query-surface.md:175` through `docs/adr/ADR-022-events-query-surface.md:183` define canonical `EventFilter` fields including `kinds`, `verbs`, `actors`, `substrates`, `after`, `before`, `session_id`, `observed`, and `selected`.
- `docs/adr/ADR-041-event-provenance-projection.md:285` through `docs/adr/ADR-041-event-provenance-projection.md:291` add `observed`, `selected`, and `session_id`.
- `docs/adr/ADR-046-event-sourced-proposals.md:287` through `docs/adr/ADR-046-event-sourced-proposals.md:295` add `payload_proposal_id`.
- `crates/khive-storage/src/event.rs:157` through `crates/khive-storage/src/event.rs:168` defines the storage-side fields.
- `crates/khive-pack-kg/src/handlers.rs:205` through `crates/khive-pack-kg/src/handlers.rs:225` only accepts `verb`, `verbs`, `outcome`, single `actor`, single `substrate`, `since`, and `until`; it has no event `kind`/`kinds`, `ids`, `actors`, `substrates`, `session_id`, `observed`, `selected`, or `payload_proposal_id`.
- `crates/khive-pack-kg/src/handlers.rs:475` through `crates/khive-pack-kg/src/handlers.rs:482` builds an `EventFilter` with only verbs, one substrate, one actor, after, and before.

Why this blocks: storage has the new filter fields, but the public verb handler silently leaves most of them unreachable. Event consumers cannot query by typed event kind, session, observed/selected provenance, or proposal id through the MCP/list surface that ADR-022/041/046 require.

Fix: extend `ListParams` and `event_filter_from_params` to map the full event filter surface, including `EventKind` parsing and multi-value actor/substrate forms, and add handler-level regression tests that fail when these parameters are ignored.

### High - `RerankExecuted` provenance projection does not decode the ADR-042 payload shape

Evidence:
- `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:252` through `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:258` define `RerankExecuted` payload fields `candidates: Vec<Uuid>`, `reranked: Vec<(Uuid, HashMap<&'static str, f32>)>`, and `final_scores: Vec<(Uuid, f32)>`.
- `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:264` through `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:267` require `Selected` rows from the rerank output order.
- `docs/adr/ADR-041-event-provenance-projection.md:176` through `docs/adr/ADR-041-event-provenance-projection.md:178` require `RerankExecuted` to project both `Candidate` and `Selected` observations.
- `crates/khive-db/src/stores/event.rs:297` through `crates/khive-db/src/stores/event.rs:300` routes `RerankExecuted` through the generic rank decoder.
- `crates/khive-db/src/stores/event.rs:314` through `crates/khive-db/src/stores/event.rs:330` accepts only arrays of UUID strings and returns an empty vector when a field is absent.
- `crates/khive-db/src/stores/event.rs:361` through `crates/khive-db/src/stores/event.rs:363` tries `selected`, then `reranked`, then `final_scores`, but the first missing `selected` field returns `Ok(Vec::new())`, so the ADR-042 fields are never consulted.
- `crates/khive-db/src/stores/event.rs:933` through `crates/khive-db/src/stores/event.rs:937` tests a synthetic `"selected": [uuid]` payload instead of the ADR-042 `final_scores` tuple payload.

Why this blocks: real ADR-042 rerank events will insert candidate rows but no selected rows. That breaks `EventFilter.selected`, provenance-aware folds, and the cluster's observable event payload contract while the current tests still pass.

Fix: make the decoder event-kind-specific. For `RerankExecuted`, parse `final_scores` as ordered `[id, score]` tuples for `Selected` rows and keep `candidates` as input candidate rows; add a regression test using the exact ADR-042 payload shape.

### High - The EventView consumer contract is implemented only as a synthetic empty dispatch hook

Evidence:
- `docs/adr/ADR-041-event-provenance-projection.md:222` through `docs/adr/ADR-041-event-provenance-projection.md:241` define `EventView` as the fold consumer surface and require runtime fetch of the event row plus matching `event_observations` before invoking `on_event`.
- `docs/adr/ADR-041-event-provenance-projection.md:584` through `docs/adr/ADR-041-event-provenance-projection.md:589` require `PackEventConsumer::on_event(&EventView)`.
- `crates/khive-runtime/src/pack.rs:30` through `crates/khive-runtime/src/pack.rs:35` exposes only `DispatchHook::on_dispatch(&EventView)`.
- `crates/khive-runtime/src/pack.rs:538` through `crates/khive-runtime/src/pack.rs:549` synthesizes an audit event and constructs `EventView { observations: Vec::new() }`; there is no persisted event lookup or JOIN with `event_observations`.
- `crates/khive-pack-brain/src/lib.rs:316` through `crates/khive-pack-brain/src/lib.rs:318` still documents a synthesized event, and `crates/khive-pack-brain/src/lib.rs:334` folds only `&view.event`.

Why this blocks: the raw `&Event` signature is gone, but the ADR-041 consumer semantics are not present. Consumers never receive persisted provenance observations through a real `PackEventConsumer::on_event` path, so the cluster only partially addresses F216.

Fix: add the actual event consumer delivery path required by ADR-041, fetch `(event, observations)` from storage before invoking consumers, and update brain/fold tests to assert non-empty provenance reaches a consumer for a projected event.

## What Looks Correct

- `crates/khive-runtime/src/operations.rs:232` through `crates/khive-runtime/src/operations.rs:239` now takes `&NamespaceToken` and passes `EventFilter` directly to storage, matching current ADR-022 wording.
- `crates/khive-db/src/stores/event.rs:994` covers deterministic event ordering by `created_at DESC, id DESC`.
- Storage-level tests cover several new filters (`kind`, `session_id`, `observed`, `selected`, `payload_proposal_id`), but the public handler and ADR-042 payload shape are not covered.

## Commands Run

Exact prompt commands from the repository root:
- `cargo fmt --all -- --check 2>&1 | tail -5`: failed because `/Users/lion/khive-work/worktrees/adr-001-015-alignment-impl-c06` has no root `Cargo.toml`.
- `cargo check --workspace 2>&1 | tail -10`: failed with Cargo exit 101 for the same missing root manifest.
- `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20`: failed with Cargo exit 101 for the same missing root manifest.
- `cargo test --workspace 2>&1 | tail -30`: failed with Cargo exit 101 for the same missing root manifest.

Equivalent workspace-manifest commands:
- `cargo fmt --manifest-path crates/Cargo.toml --all -- --check`: passed.
- `cargo check --manifest-path crates/Cargo.toml --workspace`: passed.
- `RUSTC_WRAPPER= cargo clippy --manifest-path crates/Cargo.toml --workspace --all-targets -- -D warnings`: passed. The same command without clearing `RUSTC_WRAPPER` failed because `sccache` could not run in this sandbox.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml --workspace`: passed.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-types -p khive-storage -p khive-db -p khive-runtime`: passed.
- `RUSTC_WRAPPER= make ci`: passed, including Rust tests, contract tests, Deno tests, and smoke tests.
- `git diff --check show/adr-001-015-alignment/integration...HEAD`: passed.

## Re-Review Guidance

Re-review should focus first on the migration ledger split and the public `list(kind="event")` handler surface. After those are fixed, add an ADR-042-shaped rerank event regression test and an EventView consumer test that proves projected observations reach a consumer.

Domain utility: SKIPPED - lore suggest/compose tools were not available in this session; review used the local ADR corpus and khive review skill.

VERDICT: REJECT
