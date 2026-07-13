#!/usr/bin/env python3
"""Flagship E2E result schema v3 (stdlib-only).

Implements the Distribution contract and the Ledger record contract from
`.khive/workspaces/20260711/bench-overhaul/DESIGN.md` ("Typed interfaces")
as plain-dict validators, mirroring `schemas/flagship-result-v3.json` (the
checked JSON Schema copy of the same contract, kept for external tooling
and IDE validation). This module is the one `scripts/perf/coverage_validator.py`
actually imports - no `jsonschema` dependency, matching the stdlib-only
convention of `bench_track.py` and `bench_calibrate.py`.

Measurement-evidence requirements (non-empty `distributions`, the
`successes` floor, and `runtime.daemon_fallback_count == 0`) apply only to
`status == "ok"` records (khive#945 M3): an `"error"` / `"confounded"` /
`"insufficient_samples"` record is honest failure evidence, not a
certification of measurement, so it must remain schema-valid with zero
samples and a positive fallback count. `coverage_validator.scenario_status`
already refuses to count any non-`"ok"` status as `"measured"` - this module
does not need to duplicate that rule to keep coverage correct.

`artifact.name`/`artifact.sha256` are validated for every record regardless
of status (a digest reference, when present, must be well-formed) and are
kept aligned with `schemas/flagship-result-v3.json`'s `artifactRef` def:
`name` a non-empty string, `sha256` a 64-character lowercase hex string.

`SCHEMA_VERSION = 3` here because the Ledger record contract fixes
`FlagshipRecord.schema_version` at 3 - the same schema_version track that
`bench_track.py`'s ledger will carry for flagship-e2e records once PR 2
wires this runner's output into that ledger. `bench_track.py` itself
remains at its own `SCHEMA_VERSION = 2` for its existing (non-flagship)
suites until then; the two are the same version track, not independent
ones, once flagship-e2e records land there.

No runner in this repository emits documents against this schema yet - PR 1
is contracts and a coverage validator only, per the phased plan. PR 2+
implement the scenario runner that will produce real records.
"""

from __future__ import annotations

import re

SCHEMA_VERSION = 3
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
# Global floor for status="ok" records: nearest-rank p99 needs n>=100 to
# land on a real observation rather than interpolate/repeat a low-n sample
# (khive#945 amendment, Leo signed §2.1). A scenario may declare a HIGHER
# minimum (raise-only, enforced in coverage_validator.py against the
# manifest); a declared minimum below this floor never lowers it.
TIMED_MIN_SUCCESSES = 100

SHA256_REF_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
ARTIFACT_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

# These two tuples mirror the JSON Schema exactly: for the record top level
# and for `distribution`, the schema's `properties` set equals its
# `required` set and `additionalProperties` is `false` - so "required" and
# "allowed" are the same tuple and any key outside it is a schema violation.
RECORD_FIELDS = (
    "schema_version",
    "suite",
    "scenario_id",
    "feature",
    "operation",
    "arm",
    "sha",
    "branch",
    "run_id",
    "run_attempt",
    "timestamp",
    "status",
    "metrics",
    "distributions",
    "workload",
    "runtime",
    "host",
    "settle",
    "calibration",
    "artifact",
)
DISTRIBUTION_FIELDS = (
    "estimator",
    "unit",
    "attempts",
    "successes",
    "timed_out",
    "errors_by_code",
    "histogram_edges_us",
    "histogram_counts",
    "p50_us",
    "p95_us",
    "p99_us",
    "max_us",
    "conditional_on_success",
)
WORKLOAD_REQUIRED_FIELDS = (
    "manifest_version",
    "manifest_hash",
    "scenario_id",
    "fixture",
    "fixture_hash",
    "scale",
    "concurrency",
    "attempts",
)
ARTIFACT_FIELDS = ("name", "sha256")


def _err(path: str, message: str) -> str:
    return f"{path}: {message}"


def _is_nonneg_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _check_closed_object(obj: dict, allowed: tuple, path: str, errors: list[str]) -> None:
    missing = [f for f in allowed if f not in obj]
    if missing:
        errors.append(_err(path, f"missing required field(s): {', '.join(missing)}"))
    extra = sorted(set(obj) - set(allowed))
    if extra:
        errors.append(_err(path, f"unexpected field(s) not allowed: {', '.join(extra)}"))


def validate_distribution(dist: dict, path: str = "distribution", require_evidence: bool = True) -> list[str]:
    """Validate one Distribution contract instance. Returns a list of human
    -readable error strings; an empty list means the distribution is valid.

    `require_evidence` gates the error/timeout-accounting field requirement
    (`errors_by_code`, `timed_out`) - `validate_record` passes `False` for
    non-`"ok"` records (khive#945 M3), since an honest error/confounded
    record need not carry full measurement accounting. The `successes`
    floor is enforced by `validate_record` itself (khive#945 item 4), not
    here, since it also needs the manifest-declared raise-only minimum that
    only `coverage_validator.py` has access to.
    """
    errors: list[str] = []
    if not isinstance(dist, dict):
        return [_err(path, f"expected object, got {type(dist).__name__}")]

    required_fields = DISTRIBUTION_FIELDS if require_evidence else tuple(
        f for f in DISTRIBUTION_FIELDS if f not in ("errors_by_code", "timed_out")
    )
    missing = [f for f in required_fields if f not in dist]
    if missing:
        errors.append(_err(path, f"missing required field(s): {', '.join(missing)}"))
    extra = sorted(set(dist) - set(DISTRIBUTION_FIELDS))
    if extra:
        errors.append(_err(path, f"unexpected field(s) not allowed: {', '.join(extra)}"))

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
        if name in dist and not _is_nonneg_int(value):
            errors.append(_err(path, f"{name} must be a non-negative integer, got {value!r}"))

    errors_by_code = dist.get("errors_by_code")
    errors_by_code_valid = isinstance(errors_by_code, dict)
    if not errors_by_code_valid:
        errors.append(_err(path, "errors_by_code must be an object"))
    else:
        for code, count in errors_by_code.items():
            if not isinstance(code, str):
                errors.append(_err(path, f"errors_by_code key must be a string, got {code!r}"))
                errors_by_code_valid = False
            if not _is_nonneg_int(count):
                errors.append(_err(path, f"errors_by_code[{code!r}] must be a non-negative integer, got {count!r}"))
                errors_by_code_valid = False

    if isinstance(attempts, int) and isinstance(successes, int) and isinstance(timed_out, int) and errors_by_code_valid:
        errors_total = sum(errors_by_code.values())
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
    else:
        if len(edges) != len(counts):
            errors.append(
                _err(
                    path, f"histogram_edges_us ({len(edges)}) and histogram_counts ({len(counts)}) must be the same length"
                )
            )
        for idx, edge in enumerate(edges):
            if not _is_nonneg_int(edge):
                errors.append(_err(path, f"histogram_edges_us[{idx}] must be a non-negative integer, got {edge!r}"))
        for idx, count in enumerate(counts):
            if not _is_nonneg_int(count):
                errors.append(_err(path, f"histogram_counts[{idx}] must be a non-negative integer, got {count!r}"))

    for name in ("p50_us", "p95_us", "p99_us", "max_us"):
        value = dist.get(name)
        if value is not None and not _is_nonneg_int(value):
            errors.append(_err(path, f"{name} must be null or a non-negative integer, got {value!r}"))

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

    _check_closed_object(record, RECORD_FIELDS, "record", errors)

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

    # operation and arm are denormalized from the manifest so hotspot
    # ranking (largest stable gap by verb/workload/percentile) never
    # parses scenario_id strings.
    for field in ("operation", "arm"):
        if not isinstance(record.get(field), str) or not record.get(field):
            errors.append(_err(field, "must be a non-empty string"))
    if isinstance(scenario_id, str):
        parts = scenario_id.split(".")
        arm = record.get("arm")
        if len(parts) < 3:
            errors.append(_err("scenario_id", f"must have at least 3 dot-separated segments, got {scenario_id!r}"))
        elif isinstance(arm, str) and arm and arm != parts[2]:
            errors.append(_err("arm", f"must match the scenario_id arm segment {parts[2]!r}, got {arm!r}"))

    sha = record.get("sha")
    if not isinstance(sha, str) or len(sha) != 40:
        errors.append(_err("sha", "must be a full 40-character git sha"))

    for field in ("branch", "run_id", "run_attempt", "timestamp"):
        if not isinstance(record.get(field), str) or not record.get(field):
            errors.append(_err(field, "must be a non-empty string"))

    metrics = record.get("metrics")
    if not isinstance(metrics, dict):
        errors.append(_err("metrics", "must be an object"))

    is_ok = record.get("status") == "ok"

    distributions = record.get("distributions")
    if not isinstance(distributions, dict):
        errors.append(_err("distributions", "must be an object"))
    elif is_ok and not distributions:
        errors.append(
            _err(
                "distributions",
                "must contain at least one distribution when status is 'ok' - an empty distributions "
                "object carries no measurement evidence and cannot certify a scenario as measured",
            )
        )
    else:
        for name, dist in distributions.items():
            errors.extend(validate_distribution(dist, path=f"distributions.{name}", require_evidence=is_ok))
            if is_ok and isinstance(dist, dict):
                successes = dist.get("successes")
                if _is_nonneg_int(successes) and successes < TIMED_MIN_SUCCESSES:
                    errors.append(
                        _err(
                            f"distributions.{name}.successes",
                            f"must be >= {TIMED_MIN_SUCCESSES} for status='ok' records - nearest-rank p99 "
                            "requires n>=100 to land on a real observation",
                        )
                    )

    workload = record.get("workload")
    if not isinstance(workload, dict):
        errors.append(_err("workload", "must be an object"))
    else:
        for field in WORKLOAD_REQUIRED_FIELDS:
            if field not in workload:
                errors.append(_err(f"workload.{field}", "is required"))
        if "manifest_version" in workload and not isinstance(workload["manifest_version"], str):
            errors.append(_err("workload.manifest_version", "must be a string"))
        for hash_field in ("manifest_hash", "fixture_hash"):
            if hash_field in workload and not SHA256_REF_RE.match(str(workload[hash_field])):
                errors.append(
                    _err(f"workload.{hash_field}", f"must match 'sha256:<64 lowercase hex>', got {workload[hash_field]!r}")
                )
        if "fixture" in workload and not isinstance(workload["fixture"], str):
            errors.append(_err("workload.fixture", "must be a string"))
        if "scenario_id" in workload and not isinstance(workload["scenario_id"], str):
            errors.append(_err("workload.scenario_id", "must be a string"))
        if "scale" in workload and not isinstance(workload["scale"], dict):
            errors.append(_err("workload.scale", "must be an object"))
        concurrency = workload.get("concurrency")
        if "concurrency" in workload and (not isinstance(concurrency, int) or isinstance(concurrency, bool) or concurrency < 1):
            errors.append(_err("workload.concurrency", f"must be an integer >= 1, got {concurrency!r}"))
        attempts_w = workload.get("attempts")
        if "attempts" in workload and (not isinstance(attempts_w, int) or isinstance(attempts_w, bool) or attempts_w < 1):
            errors.append(_err("workload.attempts", f"must be an integer >= 1, got {attempts_w!r}"))

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
        if "daemon_fallback_count" not in runtime:
            errors.append(_err("runtime.daemon_fallback_count", "is required"))
        elif not _is_nonneg_int(runtime["daemon_fallback_count"]):
            errors.append(
                _err(
                    "runtime.daemon_fallback_count",
                    f"must be a non-negative integer, got {runtime['daemon_fallback_count']!r}",
                )
            )
        elif is_ok and runtime["daemon_fallback_count"] > 0:
            errors.append(
                _err(
                    "runtime.daemon_fallback_count",
                    f"must be 0 for status='ok' records, got {runtime['daemon_fallback_count']!r} - fallback-engaged "
                    "runs cannot be certified as measured until a positive daemon-engagement proof exists",
                )
            )

    host = record.get("host")
    if not isinstance(host, dict):
        errors.append(_err("host", "must be an object"))
    else:
        for field in ("os", "arch"):
            if not isinstance(host.get(field), str) or not host.get(field):
                errors.append(_err(f"host.{field}", "must be a non-empty string"))
        if "cpu_count" not in host:
            errors.append(_err("host.cpu_count", "is required"))
        elif host["cpu_count"] is not None and (isinstance(host["cpu_count"], bool) or not isinstance(host["cpu_count"], int)):
            errors.append(_err("host.cpu_count", f"must be null or an integer, got {host['cpu_count']!r}"))

    settle = record.get("settle")
    if not isinstance(settle, dict):
        errors.append(_err("settle", "must be an object"))
    else:
        if settle.get("method") not in SETTLE_METHODS:
            errors.append(_err("settle.method", f"must be one of {SETTLE_METHODS}, got {settle.get('method')!r}"))
        if settle.get("state") not in STATES:
            errors.append(_err("settle.state", f"must be one of {STATES}, got {settle.get('state')!r}"))

    calibration = record.get("calibration")
    if calibration is not None:
        if not isinstance(calibration, dict):
            errors.append(_err("calibration", "must be null or an object"))
        else:
            for field in ("artifact_sha", "baseline_id", "tolerance_factor", "same_sha_run_count"):
                if field not in calibration:
                    errors.append(_err(f"calibration.{field}", "is required when calibration is not null"))
            same_sha_run_count = calibration.get("same_sha_run_count")
            if "same_sha_run_count" in calibration and (
                not isinstance(same_sha_run_count, int) or isinstance(same_sha_run_count, bool) or same_sha_run_count < 10
            ):
                errors.append(_err("calibration.same_sha_run_count", f"must be an integer >= 10, got {same_sha_run_count!r}"))

    artifact = record.get("artifact")
    if not isinstance(artifact, dict):
        errors.append(_err("artifact", "must be an object"))
    else:
        _check_closed_object(artifact, ARTIFACT_FIELDS, "artifact", errors)
        name = artifact.get("name")
        if not isinstance(name, str) or not name:
            errors.append(_err("artifact.name", f"must be a non-empty string, got {name!r}"))
        artifact_sha256 = artifact.get("sha256")
        if not isinstance(artifact_sha256, str) or not ARTIFACT_SHA256_RE.match(artifact_sha256):
            errors.append(
                _err(
                    "artifact.sha256",
                    f"must be a 64-character lowercase hex string, got {artifact_sha256!r}",
                )
            )

    return errors
