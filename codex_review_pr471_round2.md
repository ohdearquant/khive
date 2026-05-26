Verdict: REJECT
Findings: 0 Blocker, 3 High, 0 Medium, 0 Low

Round-2 fixed the original `khive-request` `"complete"` fixture failure: `cargo test --workspace` and `cargo test -p khive-request` now pass. I cannot approve this round because required contract verification did not pass, and there are still stale bare non-KG verb examples in accepted ADRs and parser fixtures.

## Findings

### [High] Accepted ADRs Still Contain Bare Pack-Verb Call Examples

Evidence: `docs/adr/ADR-023-declarative-pack-format.md:158` says every non-kg pack prefixes verbs with `pack.verb`, and `docs/adr/ADR-023-declarative-pack-format.md:180` says CI enforces that contract. But accepted ADR snippets still use bare calls: `docs/adr/ADR-019-gtd-pack.md:38` has `assign(title="x")`, `docs/adr/ADR-019-gtd-pack.md:422` has `transition(id, status="active")`, `docs/adr/ADR-019-gtd-pack.md:439` has `tasks(...)` and `next()`, `docs/adr/ADR-021-memory-pack.md:203` has `recall(query, ...)`, `docs/adr/ADR-021-memory-pack.md:230` has `recall(kind="memory")`, `docs/adr/ADR-021-memory-pack.md:305` has `remember(content=...)`, `docs/adr/ADR-040-communication-and-schedule-packs.md:110` has `reply(id)`, `docs/adr/ADR-040-communication-and-schedule-packs.md:260` has `agenda()`, and `docs/adr/ADR-040-communication-and-schedule-packs.md:473` has `send(to="agent:khive")`.

Why this matters: These are not historical "before" examples; they appear in normative accepted ADR text. This directly undercuts the claimed ADR update and gives downstream implementers stale wire examples after the migration.

Suggested fix: Prefix all non-KG call examples and verb references in these ADRs, e.g. `gtd.assign`, `gtd.transition`, `gtd.tasks`, `gtd.next`, `memory.recall`, `memory.remember`, `comm.reply`, `schedule.agenda`, `schedule.remind`, `schedule.schedule`, and `comm.send`.

### [High] `khive-request` Parser Fixtures Still Use Bare `assign(...)`

Evidence: `crates/khive-request/src/lib.rs:1274` parses `assign(title="root") | assign(...)`; the same bare fixture pattern remains at `crates/khive-request/src/lib.rs:1295`, `crates/khive-request/src/lib.rs:1312`, `crates/khive-request/src/lib.rs:1339`, `crates/khive-request/src/lib.rs:1360`, and `crates/khive-request/src/lib.rs:1367`. ADR-023 requires non-KG pack verbs to be pack-prefixed (`docs/adr/ADR-023-declarative-pack-format.md:158`).

Why this matters: The round-1 blocker was a stale `khive-request` DSL fixture. The crate now passes, but it still preserves bare GTD examples in parser tests, so this fix is incomplete and future parser examples can keep normalizing the wrong surface.

Suggested fix: Change these fixtures and adjacent comments to `gtd.assign(...)`. If parser tests intentionally allow arbitrary identifiers, add a separate neutral non-product identifier instead of using a migrated product verb bare.

### [High] Required Python Contract Gate Did Not Pass

Evidence: `tests/khive-contract/khive_contract/schema.py:11` imports `jsonschema`; `tests/khive-contract/pyproject.toml:5`-`8` declares `jsonschema>=4` as a package dependency. Running the requested `pytest` command from `tests/khive-contract/` failed during collection with `ModuleNotFoundError: No module named 'jsonschema'` in `tests/test_adr_020_request_dsl.py` and `tests/test_chain_mode.py`.

Why this matters: The user explicitly required this gate to pass. Without a successful full contract run, the round-2 Python contract claims are not verified. The package README says to run `uv run pytest`, but plain `pytest` is the requested gate and does not work in the current environment.

Suggested fix: Make the required contract-test invocation reproducible in CI/review docs. Either run the gate through the project-managed environment (`uv run pytest`) and document that as the required command, or ensure the plain `pytest` environment installs `pyproject.toml` dependencies before collection. Then rerun the full suite.

## Looks Right

- `HEAD` is `60b9586` on `v025-w4-verb-migration`, matching the claimed round-2 fix commit.
- `cargo test --workspace` passes from `crates/` with `RUSTC_WRAPPER=` empty.
- `cargo test -p khive-request` passes: 51 unit tests and doc-tests pass.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` passes.
- `python3 tests/smoke_test.py` passes, including KG, GTD dotted verbs, and memory dotted verbs.
- `tests/khive-contract/tests/test_no_bare_non_kg_verbs.py` passes directly (`2 passed`) and `KG_SUBSTRATE_BARE` contains all 14 allowed bare verbs: `create`, `delete`, `get`, `link`, `list`, `merge`, `neighbors`, `propose`, `query`, `review`, `search`, `traverse`, `update`, `withdraw`.
- I injected a bad manifest set containing bare `assign`; `test_all_product_verbs_follow_namespace_contract()` failed as expected.
- `crates/khive-mcp/src/tools/request.rs:13`-`17` now shows `gtd.next`, `gtd.assign`, and JSON `gtd.complete`.
- `crates/khive-pack-schedule/src/lib.rs:60` now uses `schedule.remind(...)` in the scheduled action parameter example.

## Commands Run

- `date -Iseconds`: `2026-05-25T23:13:32-04:00` at start.
- `git status --short --branch`: `## v025-w4-verb-migration...origin/v025-w4-verb-migration`.
- `git log --oneline --decorate -8`: confirmed `60b9586` at HEAD.
- `RUSTC_WRAPPER= cargo test --workspace` from `crates/`: passed.
- Last 30-line tail requested for `cargo test --workspace`:

```text
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests khive_types

running 1 test
test khive-types/src/pack.rs - pack::EdgeEndpointRule (line 169) ... ignored

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests khive_vcs

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests khive_vcs_adapters

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests kkernel

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

- `RUSTC_WRAPPER= cargo test -p khive-request` from `crates/`: passed, `51 passed; 0 failed`.
- `RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings` from `crates/`: passed.
- `cargo fmt --all -- --check` from `crates/`: passed.
- `python3 tests/smoke_test.py`: passed; output ended with `MEMORY PACK SMOKE TESTS PASSED`.
- `pytest` from `tests/khive-contract/`: failed during collection due missing `jsonschema`.
- `pytest tests/test_no_bare_non_kg_verbs.py -q` from `tests/khive-contract/`: passed, `2 passed in 0.01s`.
- `python3.12` bad-input check for `test_no_bare_non_kg_verbs.py`: `KG_SUBSTRATE_BARE count: 14`; injected bare `assign` failed as expected.
- Prescribed stale sweep:
  `rg -n '"(assign|next|complete|tasks|transition|remember|recall|send|inbox|read|reply|remind|schedule|agenda|cancel|learn|cite|topic)"\s*(=>|\)|:)' --glob '!docs/_archive/**' --glob '!target/**'`
  produced many hits; status values such as `"next"`, `"inbox"`, `"read"`, consumer-kind strings, and dotted examples were not treated as verb-name defects. Additional invocation sweep found the stale ADR and parser fixture evidence above.
- `UV_CACHE_DIR=/tmp/uv-cache uv run --offline pytest`: failed because network is disabled and the requested wheel was not in cache; this did not verify the contract suite.

## What I Did Not Check

- I did not fetch PR metadata from GitHub; this review is against local `HEAD` at `60b9586`.
- I did not install Python dependencies because network/cache access is restricted in this sandbox.
- I did not perform a full semantic review of unrelated brain/retrieval ADRs beyond the verb-namespace sweep and the files implicated by the round-2 claims.

## Re-Review Guidance

Do a narrow re-review after the stale ADR/parser fixtures are corrected and the full `tests/khive-contract` suite is run successfully in a reproducible environment. The key checks are the same: `cargo test --workspace`, clippy, fmt, smoke test, full contract pytest, and a broad stale bare-pack-verb sweep.

Domain utility: SKIPPED — lore/domain MCP tools were not available in this environment; I used the khive PR review skill and local ADR/code evidence instead.
