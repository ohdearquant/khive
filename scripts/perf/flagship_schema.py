#!/usr/bin/env python3
"""Flagship E2E result schema v1 (stdlib-only).

Implements the Distribution contract and the Ledger record contract from
`.khive/workspaces/20260711/bench-overhaul/DESIGN.md` ("Typed interfaces")
as plain-dict validators, mirroring `schemas/flagship-result-v1.json` (the
checked JSON Schema copy of the same contract, kept for external tooling
and IDE validation). This module is the one `scripts/perf/coverage_validator.py`
actually imports - no `jsonschema` dependency, matching the stdlib-only
convention of `bench_track.py` and `bench_calibrate.py`.

This is a standalone schema for the flagship-e2e result document. Its own
version track (`SCHEMA_VERSION = 1`, this module) is independent of
`bench_track.py`'s ledger `schema_version` integer (currently 2; DESIGN.md
plans a v3 there when PR 2 wires flagship-e2e records into that ledger).

No runner in this repository emits documents against this schema yet - PR 1
is contracts and a coverage validator only, per the phased plan. PR 2+
implement the scenario runner that will produce real records.
"""

from __future__ import annotations

SCHEMA_VERSION = 1
SUITE_NAME = "flagship-e2e"

FEATURES = ("F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10")
SURFACES = ("mcp_daemon", "admin_cli", "raw_daemon_control")
EMBEDDERS = ("none", "bench_hash", "production")
STATES = ("cold", "settling", "warm")
SETTLE_METHODS = ("none", "sequential_sentinel", "explicit_lifecycle_predicate")
RUNNER_CLASSES = ("hosted_hash", "self_hosted_cpu", "self_hosted_real_embedder")
RECORD_STATUSES = ("ok", "error", "confounded", "insufficient_samples")
ESTIMATOR = "nearest_rank_v1"
DISTRIBUTION_UNIT = "us"


def _err(path: str, message: str) -> str:
    return f"{path}: {message}"


def validate_distribution(dist: dict, path: str = "distribution") -> list[str]:
    """Validate one Distribution contract instance. Returns a list of human
    -readable error strings; an empty list means the distribution is valid.
    """
    errors: list[str] = []
    if not isinstance(dist, dict):
        return [_err(path, f"expected object, got {type(dist).__name__}")]

    if dist.get("estimator") != ESTIMATOR:
        errors.append(_err(path, f"estimator must be {ESTIMATOR!r}, got {dist.get('estimator')!r}"))
    if dist.get("unit") != DISTRIBUTION_UNIT:
        errors.append(_err(path, f"unit must be {DISTRIBUTION_UNIT!r}, got {dist.get('unit')!r}"))
    if dist.get("conditional_on_success") is not True:
        errors.append(_err(path, "conditional_on_success must be true"))

    attempts = dist.get("attempts")
    successes = dist.get("successes")
    timed_out = dist.get("timed_out")
    for name, value in (("attempts", attempts), ("successes", successes), ("timed_out", timed_out)):
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            errors.append(_err(path, f"{name} must be a non-negative integer, got {value!r}"))

    if isinstance(attempts, int) and isinstance(successes, int) and isinstance(timed_out, int):
        errors_total = sum(dist.get("errors_by_code", {}).values()) if isinstance(dist.get("errors_by_code"), dict) else 0
        if successes + timed_out + errors_total > attempts:
            errors.append(
                _err(
                    path,
                    "successes + timed_out + sum(errors_by_code) must not exceed attempts "
                    f"({successes} + {timed_out} + {errors_total} > {attempts})",
                )
            )

    edges = dist.get("histogram_edges_us")
    counts = dist.get("histogram_counts")
    if not isinstance(edges, list) or not isinstance(counts, list):
        errors.append(_err(path, "histogram_edges_us and histogram_counts must be arrays"))
    elif len(edges) != len(counts):
        errors.append(
            _err(path, f"histogram_edges_us ({len(edges)}) and histogram_counts ({len(counts)}) must be the same length")
        )

    percentiles = [dist.get("p50_us"), dist.get("p95_us"), dist.get("p99_us")]
    numeric_percentiles = [p for p in percentiles if p is not None]
    if numeric_percentiles != sorted(numeric_percentiles):
        errors.append(_err(path, "p50_us <= p95_us <= p99_us must hold when all are non-null"))
    max_us = dist.get("max_us")
    if max_us is not None and numeric_percentiles and max_us < max(numeric_percentiles):
        errors.append(_err(path, "max_us must be >= the largest non-null percentile"))

    return errors


def validate_record(record: dict) -> list[str]:
    """Validate one FlagshipRecord (Ledger record contract). Returns a list
    of human-readable error strings; an empty list means the record is
    schema-valid. Does not check freshness or manifest cross-references -
    that is `coverage_validator.py`'s job, one layer up.
    """
    errors: list[str] = []
    if not isinstance(record, dict):
        return ["record: expected object"]

    if record.get("schema_version") != SCHEMA_VERSION:
        errors.append(_err("schema_version", f"must be {SCHEMA_VERSION}, got {record.get('schema_version')!r}"))
    if record.get("suite") != SUITE_NAME:
        errors.append(_err("suite", f"must be {SUITE_NAME!r}, got {record.get('suite')!r}"))

    if record.get("feature") not in FEATURES:
        errors.append(_err("feature", f"must be one of {FEATURES}, got {record.get('feature')!r}"))
    if record.get("status") not in RECORD_STATUSES:
        errors.append(_err("status", f"must be one of {RECORD_STATUSES}, got {record.get('status')!r}"))

    scenario_id = record.get("scenario_id")
    if not isinstance(scenario_id, str) or not scenario_id:
        errors.append(_err("scenario_id", "must be a non-empty string"))

    sha = record.get("sha")
    if not isinstance(sha, str) or len(sha) != 40:
        errors.append(_err("sha", "must be a full 40-character git sha"))

    for field in ("branch", "run_id", "run_attempt", "timestamp"):
        if not isinstance(record.get(field), str) or not record.get(field):
            errors.append(_err(field, "must be a non-empty string"))

    metrics = record.get("metrics")
    if not isinstance(metrics, dict):
        errors.append(_err("metrics", "must be an object"))

    distributions = record.get("distributions")
    if not isinstance(distributions, dict):
        errors.append(_err("distributions", "must be an object"))
    else:
        for name, dist in distributions.items():
            errors.extend(validate_distribution(dist, path=f"distributions.{name}"))

    workload = record.get("workload")
    if not isinstance(workload, dict):
        errors.append(_err("workload", "must be an object"))
    else:
        for field in ("scenario_id", "fixture", "fixture_hash", "scale", "concurrency", "attempts"):
            if field not in workload:
                errors.append(_err(f"workload.{field}", "is required"))

    runtime = record.get("runtime")
    if not isinstance(runtime, dict):
        errors.append(_err("runtime", "must be an object"))
    else:
        if runtime.get("surface") not in SURFACES:
            errors.append(_err("runtime.surface", f"must be one of {SURFACES}, got {runtime.get('surface')!r}"))
        if runtime.get("embedder") not in EMBEDDERS:
            errors.append(_err("runtime.embedder", f"must be one of {EMBEDDERS}, got {runtime.get('embedder')!r}"))
        if runtime.get("runner_class") not in RUNNER_CLASSES:
            errors.append(
                _err("runtime.runner_class", f"must be one of {RUNNER_CLASSES}, got {runtime.get('runner_class')!r}")
            )

    settle = record.get("settle")
    if not isinstance(settle, dict):
        errors.append(_err("settle", "must be an object"))
    else:
        if settle.get("method") not in SETTLE_METHODS:
            errors.append(_err("settle.method", f"must be one of {SETTLE_METHODS}, got {settle.get('method')!r}"))
        if settle.get("state") not in STATES:
            errors.append(_err("settle.state", f"must be one of {STATES}, got {settle.get('state')!r}"))

    calibration = record.get("calibration")
    if calibration is not None and not isinstance(calibration, dict):
        errors.append(_err("calibration", "must be null or an object"))

    artifact = record.get("artifact")
    if not isinstance(artifact, dict) or "name" not in artifact or "sha256" not in artifact:
        errors.append(_err("artifact", "must be an object with name and sha256"))

    return errors
