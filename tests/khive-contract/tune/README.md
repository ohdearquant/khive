# Recall parameter tuning

Grid-search runner for khive recall configuration. Operates against a live
MCP session via the `khive_contract.client.KhiveMcpSession` harness (provided
by the parent `tests/khive-contract/` package).

## Prerequisites

This script depends on the `khive_contract` Python package in the parent
directory. Install it first:

```bash
cd tests/khive-contract
uv pip install -e .
```

You'll also need the `khive-mcp` binary on your PATH (the tests/khive-contract
harness spawns it via stdio).

## Run

```bash
cd tests/khive-contract
uv run python -m tune --quick                    # ~10 sec, every 10th config
uv run python -m tune                            # ~2 min, all 116 configs
uv run python -m tune --output-dir /tmp/my-run   # custom output location
```

## Outputs

- `results.json` — all (config, recall@10) tuples
- `tuned-config.toml` — recommended config (synthesized from the best-scoring
  set; see REPORT.md for honesty about how meaningful this is)
- `REPORT.md` — analysis writeup

## Known limitation

The synthetic eval corpus (`fixtures/memories_corpus.json`) has a ceiling at
recall@10 = 0.9333 for **every** config — i.e., the queries are too easy to
discriminate between parameters. Until a harder corpus exists (embed-enabled,
synonym queries, partial matches), the grid runs but cannot ground default
changes. `RecallConfig::default()` was intentionally NOT changed in this PR.
