"""Recall tuning-grid defaults contract tests.

ADR: ADR-033
section: 1. RecallConfig — all weights are parameters

Pins the tuning grid to the shipped `RecallConfig` defaults, so a grid that no
longer covers what production actually serves cannot silently report a tuning
result for a configuration space the runtime never occupies. Specifically:
`fuse_strategy` default `Rrf { k: 10 }`, `candidate_multiplier` default 20, and
an explicit `candidate_limit` on every candidate pool. Also covers that a tuned
config round-trips `candidate_limit`, and that the runtime-default baseline
sends no request-level config at all.
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Any

from tune.grid_search import (
    evaluate_config,
    generate_grid,
    load_grid_dimensions,
    write_results,
)

# Dotted pack.verb form: `memory.recall` is the product wire verb (the bare
# `recall` name is not on the memory pack's dispatch surface), matching the
# sibling corpus contract's declaration and the name the live harness sends.
VERBS_UNDER_TEST = {"memory.recall"}


class RecordingSession:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def verb(self, verb: str, args: dict[str, Any]) -> list[dict[str, Any]]:
        self.calls.append((verb, args))
        return []


def test_grid_contains_shipped_rrf_and_candidate_pool_defaults() -> None:
    dimensions = load_grid_dimensions()
    assert {"rrf": {"k": 10}} in dimensions["fusion_configs"]
    assert {
        "candidate_multiplier": 20,
        "candidate_limit": 150,
    } in dimensions["candidate_pools"]
    assert all(pool["candidate_limit"] is not None for pool in dimensions["candidate_pools"])

    grid = generate_grid()
    assert len(grid) == 1296
    assert any(config["fuse_strategy"] == {"rrf": {"k": 10}} for config in grid)
    assert any(
        config["candidate_multiplier"] == 20 and config["candidate_limit"] == 150 for config in grid
    )
    assert len(generate_grid(quick=True)) == 130


def test_tuned_config_round_trips_candidate_limit(tmp_path: Path) -> None:
    config = generate_grid()[0]
    result = {
        "config_index": 0,
        "config": config,
        "recall_at_10": 1.0,
        "mrr": 1.0,
        "mean_latency_ms": 1.0,
    }

    write_results(
        [result],
        tmp_path,
        t_total_seconds=1.0,
        n_eval_queries=1,
    )

    tuned = tomllib.loads((tmp_path / "tuned-config.toml").read_text())
    assert tuned["recall"]["candidate_limit"] == config["candidate_limit"]


def test_runtime_default_baseline_omits_request_config() -> None:
    session = RecordingSession()
    metrics = evaluate_config(
        session,  # type: ignore[arg-type]
        None,
        [{"query": "runtime default", "relevant_indices": []}],
        {},
    )

    assert metrics["recall_at_10"] == 0.0
    assert session.calls == [("memory.recall", {"query": "runtime default", "limit": 10})]
