# khive-contract

ADR-organized contract tests for the MCP surface, served by the `kkernel mcp` subcommand.

## What this is

This package converts the single-file `tests/contract_test.py` and `tests/smoke_test.py` into a
proper uv-managed Python package with:

- Tests organized by ADR
- Function-scoped fixtures with private file-backed stores
- pytest-benchmark latency baselines
- Golden snapshot comparisons
- A test manifest that verifies all 18 product verbs are hit

## How to run

Select an existing, qualified executable explicitly with `KKERNEL_BINARY`. The complete
suite requires an executable built with `kkernel/pack-formal`; a default-feature binary
cannot validate the formal ontology tests. From the repository root:

```sh
KKERNEL_BINARY=/path/to/qualified/kkernel uv run --project tests/khive-contract pytest -q tests/khive-contract
KKERNEL_BINARY=/path/to/qualified/kkernel uv run --no-project python tests/contract_test.py

# Lightweight harness checks use owned Python fake executables, never kkernel.
uv run --project tests/khive-contract python scripts/tests/test_contract_harness.py
uv run --no-project python scripts/tests/test_binary_resolution.py
```

The corresponding existing CI entry points are `scripts/ci.sh contract-tests` and
`scripts/ci.sh contract-suite` (run from the repository root, with the same explicit binary;
the script enters `crates/` itself).
Fake checks establish provisioning, serialization and child cleanup; they do not
establish product authorization, migrations, search indexing or namespace behavior.

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
3. `<repo-root>/crates/target/release/kkernel`, then `<repo-root>/crates/target/debug/kkernel`

The shared resolver also honors `CARGO_TARGET_DIR`, relative to `crates/` when it is a
relative path. The session invokes the selected file as `kkernel mcp …`, with daemon
use disabled. If the binary is missing, obtain the required feature build before running
the suite:

```bash
cd crates && cargo build --release -p kkernel --features pack-formal
```

## Fixture isolation and authorization

`tests/contract_harness.py` creates a private temporary directory, child HOME, TOML
config and database for each test function. Its config enrolls exactly
`lambda:contract-test` through `[gate].granted_actors`, with `grant_unattributed=false`.
Child environments discard inherited `KHIVE_*` settings before adding the owned
config, fixture actor and `KHIVE_NO_DAEMON=1`. The parent environment is unchanged.
Initialize and request deadlines include partial lines, notifications and unmatched
response IDs. Shutdown waits for the owned child, killing and reaping it if needed,
before its temporary directory is removed.

`temp_namespace` supplies a unique explicit routing namespace. The registry accepts
`namespace` for read/write routing, consumes it before calling the pack handler, and
uses the resulting token to stamp writes. Omission defaults writes to `local`.
Factories and clients preserve explicit namespace arguments unchanged.

Namespace tests create alpha/beta entities and observation notes through public MCP
calls in the owned store. Positive alpha/beta list and search results make exclusion
checks nonvacuous. Default-read and configured-visible-set controls run alongside
full-ID, prefix, note and cross-namespace link access. Unenrolled-caller refusal controls
read domain records to verify no mutation, while allowing denial/configuration audit events.
No administrative import or direct SQL write prepares these fixtures.

The two-backend fixture uses the same owned config and enrollment, with `config=store.config`
and `db=None`; no `--db` override competes with its two configured SQLite paths and GTD route.

## Organization

Tests are in `tests/` and organized by ADR. The `khive_contract/` package provides:

- `client.py` — `KhiveMcpSession` subprocess/JSON-RPC wrapper
- `schema.py` — JSON schema validators for verb response shapes
- `fixtures.py` — closed-set constants (entity kinds, relations, verbs)
- `benchmark.py` — latency baseline read/write utilities

## ADR filename drift note

Some test filenames use numbers from the play specification that diverged from the final ADR
numbering in this worktree:

| File                                     | Spec filename | Actual ADR covered             |
| ---------------------------------------- | ------------- | ------------------------------ |
| `test_adr_020_request_dsl.py`            | as-requested  | ADR-016 request DSL            |
| `test_adr_027_single_tool_mcp.py`        | as-requested  | ADR-027 dynamic pack loading   |
| `test_adr_021_recall_pipeline.py`        | as-requested  | ADR-021 memory pack            |
| `test_adr_033_recall_configurability.py` | as-requested  | ADR-033 recall configurability |

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
