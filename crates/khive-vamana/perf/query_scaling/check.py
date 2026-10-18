#!/usr/bin/env python3
"""Check one cohort of archived vec_bench runs; never run a benchmark or infer a knee."""

import argparse
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import sys


class EvidenceError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise EvidenceError(message)


def number(value, name, minimum=0, maximum=None):
    require(type(value) in (int, float), f"{name}: expected number")
    require(math.isfinite(value) and value >= minimum, f"{name}: invalid value")
    require(maximum is None or value <= maximum, f"{name}: above maximum")
    return value


def integer(value, name, minimum=1):
    require(type(value) is int and value >= minimum, f"{name}: expected integer >= {minimum}")
    return value


def digest(value, size, name):
    require(isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{size}}}", value),
            f"{name}: expected {size} lowercase hex digits")
    return value


def text(value, name):
    require(isinstance(value, str) and value.strip() and value != "unknown", f"{name}: required")
    return value


def strict_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def read_json(path):
    require(path.stat().st_size <= 16 * 1024 * 1024, f"{path.name}: JSON exceeds 16 MiB")
    raw = path.read_bytes()
    return json.loads(raw, object_pairs_hook=strict_object), hashlib.sha256(raw).hexdigest()


def fit(ns, values):
    """Descriptive log-log OLS; constant-series R² is undefined, not one."""
    xs = [math.log(n) for n in ns]
    ys = [math.log(value) for value in values]
    mx, my = statistics.mean(xs), statistics.mean(ys)
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)
    residual = sum((y - my - slope * (x - mx)) ** 2 for x, y in zip(xs, ys))
    variation = sum((y - my) ** 2 for y in ys)
    return {
        "exponent": slope,
        "r_squared": None if variation <= 1e-24 else 1 - residual / variation,
        "log_rmse": math.sqrt(residual / len(ns)),
        "constant_log_rmse": math.sqrt(variation / len(ns)),
        "max_over_min": max(values) / min(values),
    }


def check_run(run, manifest):
    require(isinstance(run, dict), "run must be an object")
    require(run["schema_version"] == "1.0", "unsupported vec_bench schema")
    require(run["git_sha"] == manifest["source_commit"], "source commit mismatch")
    text(run["produced_at"], "produced_at")
    timestamp = datetime.fromisoformat(run["produced_at"].replace("Z", "+00:00"))
    require(timestamp.tzinfo is not None, "produced_at must include timezone")
    text(run["runner_os"], "runner_os")
    text(run["machine_model"], "machine_model")
    integer(run["ram_bytes"], "ram_bytes")
    number(run["loadavg1"], "loadavg1")
    dataset, config = run["dataset"], run["config"]
    expected = manifest["dataset"]
    for key in ("name", "base_blake3", "query_blake3"):
        require(dataset[key] == expected[key], f"dataset {key} mismatch")
    dim = integer(dataset["dim"], "dim")
    require(dim <= 4096, "unsupported dimension")
    base_n = integer(dataset["base_n"], "base_n")
    require(base_n <= 2**32 - 1, "base corpus exceeds u32 vector identity range")
    query_n = integer(dataset["query_n"], "query_n")
    text(dataset["normalization"], "normalization")
    require(config["k"] == 10 and type(config["k"]) is int, "expected recall@10")
    number(config["target_recall"], "target_recall", 0.95, 1)
    for key in ("max_degree", "search_list_size", "build_batch", "max_iso_beam",
                "n_gt_queries", "n_latency_queries"):
        integer(config[key], key)
    require(config["n_gt_queries"] <= query_n and config["n_latency_queries"] <= query_n,
            "query sample exceeds corpus")
    number(config["alpha"], "alpha", 1e-12)
    require(run["gt_policy"]["source"] == "brute-force recomputed on subset", "wrong oracle policy")
    rows = run["rows"]
    require(isinstance(rows, list) and len(rows) >= 4, "at least four distinct N points required")
    ns = []
    for row in rows:
        n = integer(row["n"], "n", 10)
        require(n <= base_n, "N exceeds base corpus")
        require(not ns or n > ns[-1], "N points must be strictly increasing and unique")
        ns.append(n)
        require(row["recall_saturated"] is False, "iso-recall search saturated")
        number(row["recall_at_10"], "recall_at_10", config["target_recall"], 1)
        beam = integer(row["iso_recall_beam"], "iso_recall_beam", 10)
        require(beam <= config["max_iso_beam"], "beam exceeds cap")
        require(row["gt_source"] == run["gt_policy"]["source"], "row oracle mismatch")
        latency = [number(row[key], key, 1e-12) for key in (
            "query_warm_p50_us", "query_warm_p95_us", "query_warm_p99_us", "query_warm_max_us")]
        require(latency == sorted(latency), "unordered latency quantiles")
        number(row["bruteforce_p50_us"], "bruteforce_p50_us", 1e-12)
    require(ns[-1] >= 10 * ns[0], "N range must span at least 10x")
    fits = {name: fit(ns, [row[key] for row in rows]) for name, key in (
        ("query", "query_warm_p50_us"), ("beam", "iso_recall_beam"),
        ("brute_force", "bruteforce_p50_us"))}
    return ns, fits


def check(manifest_path):
    manifest_path = Path(manifest_path).resolve()
    manifest, manifest_hash = read_json(manifest_path)
    require(type(manifest["schema_version"]) is int and manifest["schema_version"] == 1,
            "unsupported manifest schema")
    digest(manifest["source_commit"], 40, "source_commit")
    dataset = manifest["dataset"]
    require(dataset["kind"] in ("proxy", "live_kg", "comparison"), "unknown corpus kind")
    for key in ("name", "model", "model_revision", "snapshot", "query_split"):
        text(dataset[key], f"dataset.{key}")
    for key in ("base_blake3", "query_blake3"):
        digest(dataset[key], 64, key)
    entries = manifest["runs"]
    require(isinstance(entries, list) and len(entries) >= 2, "at least two separate runs required")
    results, seen_paths, seen_hashes, seen_times = [], set(), set(), set()
    reference = None
    for entry in entries:
        relative = Path(entry["file"])
        require(not relative.is_absolute() and ".." not in relative.parts, "run file must be local relative path")
        path = (manifest_path.parent / relative).resolve()
        require(path.is_relative_to(manifest_path.parent), "run file escapes manifest directory")
        require(path not in seen_paths, "same run file reused")
        seen_paths.add(path)
        run, raw_hash = read_json(path)
        require(raw_hash == digest(entry["sha256"], 64, "run sha256"), "run artifact hash mismatch")
        require(raw_hash not in seen_hashes, "duplicate run artifact")
        seen_hashes.add(raw_hash)
        ns, fits = check_run(run, manifest)
        require(run["produced_at"] not in seen_times, "duplicate run timestamp")
        seen_times.add(run["produced_at"])
        identity = (run["dataset"], run["config"], ns, run["runner_os"], run["machine_model"], run["ram_bytes"])
        if reference is None:
            reference = identity
        require(identity == reference, "runs differ in dataset/config/N/host; analyze separate cohorts")
        results.append({"file": entry["file"], "sha256": raw_hash,
                        "produced_at": run["produced_at"], "loadavg1": run["loadavg1"], "fits": fits})
    return {
        "schema_version": 1,
        "status": "artifact_consistency_valid",
        "manifest_sha256": manifest_hash,
        "source_commit": manifest["source_commit"],
        "dataset": dataset,
        "n_points": reference[2],
        "n_span": reference[2][-1] / reference[2][0],
        "runs": results,
        "shape_conclusion": "requires interpretation of repeated measurements",
        "limitations": [
            "Fits describe finite-range p50 values; no slope alone establishes sublinear growth or a plateau.",
            "Low R-squared and a narrow latency range cannot locate a knee beyond the measured N range.",
            "Run copies with changed metadata are not detectable; archive separate qualified-host executions.",
            "Input hashes bind bytes, not model identity, live-corpus origin, split independence or build provenance; verify those receipts separately.",
            "The producer's legacy decisive_question and assertions are not used as scaling evidence.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()
    try:
        report = check(args.manifest)
    except (EvidenceError, OSError, ValueError, KeyError, TypeError, OverflowError) as error:
        print(f"invalid scaling evidence: {error}", file=sys.stderr)
        return 2
    print(json.dumps(report, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
