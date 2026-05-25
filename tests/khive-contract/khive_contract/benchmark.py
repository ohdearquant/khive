"""Benchmark utilities for khive contract latency tests.

Converts pytest-benchmark stats to a baselines/latency.json file with
p50_ms and p95_ms per verb.
"""

from __future__ import annotations

import json
import statistics
from pathlib import Path
from typing import Any

# Default baseline file location (relative to package root)
_PKG_ROOT = Path(__file__).parent.parent
BASELINE_PATH = _PKG_ROOT / "baselines" / "latency.json"

# Verbs that require latency baselines
BASELINE_VERBS = ("remember", "recall", "list", "search", "query")


def record_latency(
    verb: str,
    samples_ms: list[float],
    path: Path | None = None,
) -> dict[str, float]:
    """Compute p50/p95 from *samples_ms* and write to the baseline JSON file.

    Returns ``{"p50_ms": ..., "p95_ms": ...}`` for the verb.
    """
    target = path or BASELINE_PATH
    target.parent.mkdir(parents=True, exist_ok=True)

    existing: dict[str, Any] = {}
    if target.exists():
        try:
            existing = json.loads(target.read_text())
        except (json.JSONDecodeError, OSError):
            existing = {}

    sorted_samples = sorted(samples_ms)
    n = len(sorted_samples)
    p50 = _percentile(sorted_samples, 50)
    p95 = _percentile(sorted_samples, 95)

    existing[verb] = {"p50_ms": round(p50, 3), "p95_ms": round(p95, 3), "n": n}
    target.write_text(json.dumps(existing, indent=2) + "\n")

    return {"p50_ms": p50, "p95_ms": p95}


def load_baselines(path: Path | None = None) -> dict[str, dict[str, float]]:
    """Load baseline JSON or return empty dict if file is absent."""
    target = path or BASELINE_PATH
    if not target.exists():
        return {}
    return json.loads(target.read_text())


def check_regression(
    verb: str,
    actual_ms: float,
    *,
    tolerance: float = 2.0,
    path: Path | None = None,
) -> None:
    """Raise AssertionError if *actual_ms* exceeds the baseline p95 by *tolerance*×.

    Skips silently if no baseline exists for the verb.
    """
    baselines = load_baselines(path)
    if verb not in baselines:
        return
    baseline_p95 = baselines[verb].get("p95_ms", float("inf"))
    limit = baseline_p95 * tolerance
    assert actual_ms <= limit, (
        f"Latency regression for '{verb}': {actual_ms:.1f}ms > {limit:.1f}ms "
        f"(baseline p95={baseline_p95:.1f}ms × {tolerance})"
    )


def benchmark_stats_from_pytest(benchmark_stats: Any) -> dict[str, float]:
    """Extract p50/p95 from a pytest-benchmark stats object.

    Works with both the ``stats`` dict from ``benchmark.stats`` and the
    ``BenchmarkFixture`` itself.

    Returns ``{"p50_ms": ..., "p95_ms": ...}`` with values in milliseconds.
    """
    if hasattr(benchmark_stats, "stats"):
        benchmark_stats = benchmark_stats.stats

    # pytest-benchmark stores times in seconds
    data = getattr(benchmark_stats, "data", None) or benchmark_stats.get("data", [])
    if data:
        samples_s = list(data)
    else:
        # Fall back to mean if raw data is not available
        mean_s = getattr(benchmark_stats, "mean", None) or benchmark_stats.get("mean", 0)
        samples_s = [mean_s]

    samples_ms = [s * 1000.0 for s in samples_s]
    sorted_samples = sorted(samples_ms)
    return {
        "p50_ms": round(_percentile(sorted_samples, 50), 3),
        "p95_ms": round(_percentile(sorted_samples, 95), 3),
    }


def _percentile(sorted_data: list[float], pct: int) -> float:
    if not sorted_data:
        return 0.0
    n = len(sorted_data)
    if n == 1:
        return sorted_data[0]
    rank = pct / 100.0 * (n - 1)
    lower = int(rank)
    upper = lower + 1
    if upper >= n:
        return sorted_data[-1]
    frac = rank - lower
    return sorted_data[lower] + frac * (sorted_data[upper] - sorted_data[lower])
