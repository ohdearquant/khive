# khive-contract

ADR-organized contract tests for the MCP surface, served by the `kkernel mcp` subcommand.

## What this is

This package converts the single-file `tests/contract_test.py` and `tests/smoke_test.py` into a
proper uv-managed Python package with:

- Tests organized by ADR
- Shared fixtures with namespace isolation
- pytest-benchmark latency baselines
- Golden snapshot comparisons
- A test manifest that verifies all 18 product verbs are hit

## How to run

**All commands must be run via `uv run pytest`** — plain `pytest` will fail with
`ModuleNotFoundError` because dependencies (e.g. `jsonschema`) are managed by uv, not the
system Python. This is the canonical invocation required by CI and code review gates.

```bash
cd tests/khive-contract

# All tests
uv run pytest -v

# Only a specific ADR
uv run pytest -v -m adr_002

# Benchmarks only (writes baselines/latency.json)
uv run pytest --benchmark-only -v

# Smoke tests only
uv run pytest -v -m smoke

# Skip slow subprocess tests
uv run pytest -v -m "not slow"
```

## Binary resolution

The client looks for the `kkernel` binary in this order:

1. `binary=` argument to `KhiveMcpSession`
2. `KKERNEL_BINARY` environment variable (`KHIVE_MCP_BINARY` is accepted as a deprecated alias)
3. `<repo-root>/crates/target/release/kkernel`

The session invokes it as `kkernel mcp …`. If the binary is missing, build it first:

```bash
cd crates && cargo build --release -p kkernel
```

## Organization

Tests are in `tests/` and organized by ADR. The `khive_contract/` package provides:

- `client.py` — `KhiveMcpSession` subprocess/JSON-RPC wrapper
- `schema.py` — JSON schema validators for verb response shapes
- `fixtures.py` — closed-set constants (entity kinds, relations, verbs)
- `benchmark.py` — latency baseline read/write utilities

## ADR filename drift note

Some test filenames use numbers from the play specification that diverged from the final ADR
numbering in this worktree:

| File | Spec filename | Actual ADR covered |
|------|--------------|-------------------|
| `test_adr_020_request_dsl.py` | as-requested | ADR-016 request DSL |
| `test_adr_027_single_tool_mcp.py` | as-requested | ADR-027 dynamic pack loading |
| `test_adr_021_recall_pipeline.py` | as-requested | ADR-021 memory pack |
| `test_adr_033_recall_configurability.py` | as-requested | ADR-033 recall configurability |

Each test docstring cites the actual ADR section.

## Verb coverage

The manifest covers all 18 product verbs exposed by the baseline:

- KG (11): create, get, list, update, delete, merge, search, link, neighbors, traverse, query
- GTD (5): assign, next, complete, tasks, transition
- Memory (2): remember, recall

The task text mentions 15 verbs; 18 subsumes that requirement.

## Golden update policy

Golden snapshots in `golden/` are committed with volatile fields (UUIDs, timestamps,
`created_at`, `updated_at`) scrubbed to `"<redacted>"`. To regenerate:

```bash
uv run pytest -v -m golden --update-golden
```

(The `--update-golden` flag is handled in `conftest.py`.)
