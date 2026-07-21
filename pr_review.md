Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 1 High, 0 Medium, 0 Low

### [High] `event_rows` omits request-owned event-plane writes

Evidence: `crates/khive-pack-kg/src/handlers/proposal.rs:180-184` successfully appends a `ProposalCreated` event as part of the synchronous `propose` dispatch. `crates/khive-runtime/src/curation.rs:284`, `crates/khive-runtime/src/operations.rs:3734`, `crates/khive-runtime/src/operations.rs:3985`, `crates/khive-runtime/src/operations.rs:4542`, and `crates/khive-runtime/src/operations.rs:4617` do the same for update/merge/delete request paths. None increments `UsageUnit::EventRows`. The PR's sole increment is `crates/khive-runtime/src/phase_events.rs:81-90`, whose own contract says it observes a background phase "since no dispatch is happening" (`crates/khive-runtime/src/phase_events.rs:44-46`).

Why this matters: a successful `propose`, update, merge, or delete can append a non-audit event row before the audit snapshot is frozen, but its response `usage` and the matching audit `resource.units` omit `event_rows`. That violates the executed-usage contract for request-owned event-plane rows; the enclosing audit row is already correctly excluded by freezing before `append_audit_event_best_effort`.

Suggested fix: route every request-path event append through a shared helper that increments `EventRows` only after `append_event` succeeds, while keeping the deferred enclosing-audit append outside that helper (or explicitly excluded). Cover at least `propose` and one mutation verb with an MCP-level assertion that both response usage and audit units contain `event_rows: 1`.

## Looks Right

- The seven-counter vocabulary is closed in `crates/khive-storage/src/usage.rs:28-46`; zero-count measurement serializes as `{}` and unarmed work is a no-op.
- `batch_neighbors`, `neighbors`, `neighbors_both_directions`, and `traverse` now count in their async storage methods, after the blocking reader returns. `traverse` records `raw_rows` before the BFS first-visit de-duplication (`crates/khive-db/src/stores/graph.rs:1906-2096`), so the graph-hop metric is not double-counted by runtime BFS/shortest-path code.
- Per-op MCP contexts are scoped independently in both parallel and chain paths. Chain usage is stamped before canonical `$prev` extraction and presentation (`crates/khive-mcp/src/server.rs:1036-1068`).
- Audit resource construction freezes before the enclosing audit row is appended (`crates/khive-runtime/src/pack.rs:1472-1491`), excluding that row from its own snapshot. The same frozen value is then used by envelope stamping.
- The task-local propagation helper explicitly captures and re-enters request-owned spawned children, and its storage tests cover both joined and detached spawning.

## Commands Run

- `RUSTC_WRAPPER= CARGO_TARGET_DIR=/private/tmp/khive-pr1231-review-target cargo test -p khive-storage --lib usage`: PASS — 4 passed.
- `RUSTC_WRAPPER= CARGO_TARGET_DIR=/private/tmp/khive-pr1231-review-target cargo test -p khive-mcp --test integration usage`: PASS — 3 passed.
- `RUSTC_WRAPPER= cargo fmt --all -- --check`: PASS.
- `git diff --check 43596ba5f^ 43596ba5f`: PASS.

The first test attempt was blocked before compilation by the sandbox's unavailable `sccache`; disabling only `RUSTC_WRAPPER` produced the results above with the required isolated target directory.

## What I Did Not Check

- Full workspace test suite and Clippy were not run.
- No installed commercial extension packs were available to exercise their event or ANN-consumer paths.

## Re-Review Guidance

Narrow re-review after adding the request-owned event-row accounting and an end-to-end assertion that the response and audit snapshot agree.

Domain utility: SKIPPED — the supplied contract and repository workflow were sufficient for this focused instrumentation review.
