Verdict: APPROVE
Findings: 0 Blocker, 0 High, 0 Medium, 0 Low

Round-3 commit reviewed: `4a2d15c98fe38096b23395a8acb5bf1b932174be` on `v025-w4-verb-migration`.

## Findings

No migration-blocking findings.

Suggested fixes: none.

## Evidence

- ADR-019 callable examples and handler declarations now use dotted GTD forms: `gtd.assign` / `gtd.next` / `gtd.complete` / `gtd.tasks` / `gtd.transition` at `docs/adr/ADR-019-gtd-pack.md:55`, `docs/adr/ADR-019-gtd-pack.md:223`, `docs/adr/ADR-019-gtd-pack.md:244`, and `docs/adr/ADR-019-gtd-pack.md:439`.
- ADR-021 callable examples and handler declarations now use dotted memory forms: `memory.remember` / `memory.recall` at `docs/adr/ADR-021-memory-pack.md:102`, `docs/adr/ADR-021-memory-pack.md:133`, `docs/adr/ADR-021-memory-pack.md:398`, and `docs/adr/ADR-021-memory-pack.md:437`.
- ADR-040 callable examples and handler declarations now use dotted comm/schedule forms: `comm.send`, `comm.inbox`, `schedule.agenda`, `schedule.schedule` at `docs/adr/ADR-040-communication-and-schedule-packs.md:60`, `docs/adr/ADR-040-communication-and-schedule-packs.md:65`, `docs/adr/ADR-040-communication-and-schedule-packs.md:195`, `docs/adr/ADR-040-communication-and-schedule-packs.md:260`, `docs/adr/ADR-040-communication-and-schedule-packs.md:307`, and `docs/adr/ADR-040-communication-and-schedule-packs.md:312`.
- Parser fixtures in `crates/khive-request/src/lib.rs` use dotted non-KG verbs in function-call, JSON, and chain examples, including `gtd.next()` at `crates/khive-request/src/lib.rs:912`, `gtd.assign(...)` at `crates/khive-request/src/lib.rs:921`, JSON `gtd.next` / `gtd.complete` at `crates/khive-request/src/lib.rs:991`, and chain `gtd.assign(...) | gtd.assign(...)` at `crates/khive-request/src/lib.rs:1275`.
- The contract-test README now documents `uv run pytest` as canonical at `tests/khive-contract/README.md:18` and shows the all-tests invocation at `tests/khive-contract/README.md:26`.
- The requested test headers are present: `tests/khive-contract/tests/test_adr_020_request_dsl.py:1` and `tests/khive-contract/tests/test_chain_mode.py:1`.

## Commands Run

- `date -Iseconds`: confirmed review start at `2026-05-25T23:33:59-04:00`.
- `git status --short --branch`: on `v025-w4-verb-migration...origin/v025-w4-verb-migration`; only pre-existing untracked `codex_review_pr471_round2.md` before this review artifact.
- `git rev-parse HEAD`: `4a2d15c98fe38096b23395a8acb5bf1b932174be`.
- `git diff --name-status origin/main...HEAD`: inspected changed-file scope for PR #471.
- `rg -n '\b(assign|next|complete|tasks|transition|remember|recall|send|inbox|read|reply|remind|schedule|agenda|cancel)\s*\(' docs/adr/ADR-019-gtd-pack.md docs/adr/ADR-021-memory-pack.md docs/adr/ADR-040-communication-and-schedule-packs.md crates/khive-request/src/lib.rs`: no remaining bare non-KG callable examples; matches were dotted forms.
- `rg -n 'uv run pytest' tests/khive-contract/README.md tests/khive-contract/tests/test_adr_020_request_dsl.py tests/khive-contract/tests/test_chain_mode.py`: confirmed README and headers.
- `cargo fmt --all -- --check` from `crates/`: passed.
- `cargo test --workspace` from `crates/`: passed.
- `RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings` from `crates/`: passed. Plain clippy first hit sandbox-blocked `sccache`; rerun without wrapper passed.
- `UV_CACHE_DIR=/tmp/uv-cache uv run pytest tests/khive-contract/tests/test_no_bare_non_kg_verbs.py`: passed, `2 passed`.
- `UV_CACHE_DIR=/tmp/uv-cache uv run pytest -q` from `tests/khive-contract`: reproduced the reported full-suite baseline, `28 failed, 58 passed, 2 xfailed`; failures were response-shape / manifest / chain-behavior classes, not new namespace-test failures.
- `CARGO_TARGET_DIR=/Users/lion/projects/khive/khive-verb-migration/crates/target-main-origin RUSTC_WRAPPER= cargo build -p khive-mcp` from the `origin/main` worktree: passed.
- `KHIVE_MCP_BINARY=.../target-main-origin/debug/khive-mcp python -m pytest -q -p no:cacheprovider` from the `origin/main` contract-test worktree using the already-populated contract venv: reproduced the same count, `28 failed, 56 passed, 2 xfailed`. A fresh `uv run` environment for main was blocked by network-disabled dependency resolution, so this was the viable offline comparison.

## What I Did Not Check

- I did not post to GitHub or inspect live PR comments.
- I did not do a fresh-network `uv` environment sync for the `origin/main` comparison because dependency download was blocked; the comparison used the PR checkout's already-populated contract-test virtualenv and an `origin/main` binary built from source.
- I treated bare verb names in explanatory prose as non-blocking unless they appeared as callable examples, DSL strings, handler names, or product verb declarations.

## Re-Review Guidance

No further round is needed for the round-2 H1/H2/H3 fixes. If reviewers want to clean up bare prose mentions later, that can be a narrow documentation polish pass and should not block this merge.

Domain utility: SKIPPED - No lore `suggest`/`compose` tools were available in this session; review used the khive PR review skill, ADRs, static sweeps, and local gates.
