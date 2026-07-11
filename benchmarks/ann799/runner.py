#!/usr/bin/env python3
"""ANN-799 single-process runner: timing, calibration, warm blocks, JSONL output.

Drives one adapter (see adapters/base.py) through the fixed protocol in
protocol.toml: three clean builds, calibration-split binary search for the
smallest passing search width, then ten randomized warm evaluation blocks
with raw per-query latency samples written to JSONL. Every result is
consumed (summed into an accumulator) so the interpreter cannot skip the
work a lazy or dead-code-eliminating runtime might otherwise drop -- CPython
itself never elides side-effect-free calls, but the accumulator also
doubles as the recall/consistency signal used by report.py.

One runner, one data reader (dataset.py) for every contender, per the
plan's "Metrics and common measurement boundary" section. The timed window
begins immediately before `adapter.search_one` and ends when it returns;
JSON serialization and file writes happen after the timer stops.

Usage:
    python3 benchmarks/ann799/runner.py \\
        --protocol benchmarks/ann799/protocol.toml \\
        --data "$SIFT_DIR" --out "$ANN799_ROOT/run-<ts>" \\
        --systems faiss-flat,faiss-hnswflat,faiss-ivfflat,hnswlib
"""

from __future__ import annotations

import argparse
import json
import pathlib
import random
import statistics
import subprocess
import sys
import threading
import time

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    import tomli as tomllib  # type: ignore[no-redef]

import numpy as np

sys.path.insert(0, str(pathlib.Path(__file__).parent))

import dataset as ann_dataset  # noqa: E402
from adapters import faiss_cpu, hnswlib_adapter  # noqa: E402

ADAPTER_REGISTRY = {**faiss_cpu.ADAPTERS, **hnswlib_adapter.ADAPTERS}

SCHEMA_VERSION = 2


class RssSampler:
    """Background thread polling this process's RSS via `ps` every 100ms.

    macOS has no cheap in-process peak-RSS reset primitive comparable to
    Linux's /proc/self/status VmHWM, so this samples externally via `ps`
    at the cadence the plan specifies ("sampled at 100 ms") and tracks the
    max observed value across the sampling window. It measures whole-process
    RSS, not an isolated per-call delta -- documented in README.md.
    """

    def __init__(self, interval_s: float = 0.1):
        self._interval_s = interval_s
        self._peak_bytes = 0
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._pid = str(subprocess.os.getpid())

    def _sample_once(self) -> int:
        try:
            out = subprocess.run(
                ["ps", "-o", "rss=", "-p", self._pid],
                capture_output=True,
                text=True,
                timeout=1,
            )
            rss_kb = int(out.stdout.strip() or "0")
            return rss_kb * 1024
        except Exception:
            return 0

    def _run(self) -> None:
        while not self._stop.is_set():
            self._peak_bytes = max(self._peak_bytes, self._sample_once())
            self._stop.wait(self._interval_s)

    def __enter__(self) -> "RssSampler":
        self._peak_bytes = self._sample_once()
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1)

    @property
    def peak_bytes(self) -> int:
        return self._peak_bytes


def recall_at_k(returned: np.ndarray, ground_truth_top: np.ndarray, k: int) -> float:
    """Mean overlap between `returned[:, :k]` and the frozen exact top-k IDs."""
    n = returned.shape[0]
    total = 0.0
    for i in range(n):
        got = set(int(x) for x in returned[i, :k])
        want = set(int(x) for x in ground_truth_top[i, :k])
        total += len(got & want) / k
    return total / n


def calibrate_search_width(
    adapter,
    calib_queries: np.ndarray,
    calib_gt: np.ndarray,
    lo: int,
    hi: int,
    target_recall: float,
    top_k_return: int = 100,
    calibration_k: int = 10,
) -> dict:
    """Binary-search the smallest search width with recall@10 >= target.

    Records every (width, recall) pair tested so non-monotonic behavior is
    visible rather than silently trusted, per the plan's calibration clause.
    """
    tested: list[tuple[int, float]] = []

    def eval_width(width: int) -> float:
        adapter.set_search_width(width)
        results = np.stack(
            [adapter.search_one(q, top_k_return) for q in calib_queries]
        )
        r = recall_at_k(results, calib_gt, calibration_k)
        tested.append((width, r))
        return r

    if adapter.search_param_name == "none":
        r = eval_width(lo)
        return {
            "selected_width": lo,
            "selected_recall": r,
            "tested": tested,
            "monotonic": True,
        }

    best_passing: int | None = None
    left, right = lo, hi
    r_hi = eval_width(right)
    if r_hi < target_recall:
        # Even the widest search fails the calibration target: record the
        # failure and stop rather than pretend a narrower width could pass.
        return {
            "selected_width": None,
            "selected_recall": r_hi,
            "tested": tested,
            "monotonic": True,
        }
    best_passing = right

    while left <= right:
        mid = (left + right) // 2
        if mid in (w for w, _ in tested):
            r_mid = next(r for w, r in tested if w == mid)
        else:
            r_mid = eval_width(mid)
        if r_mid >= target_recall:
            best_passing = mid
            right = mid - 1
        else:
            left = mid + 1

    tested.sort(key=lambda wr: wr[0])
    recalls = [r for _, r in tested]
    monotonic = all(recalls[i] <= recalls[i + 1] + 1e-9 for i in range(len(recalls) - 1))
    if not monotonic:
        passing = [w for w, r in tested if r >= target_recall]
        if passing:
            best_passing = min(passing)

    selected_recall = next(r for w, r in tested if w == best_passing)
    return {
        "selected_width": best_passing,
        "selected_recall": selected_recall,
        "tested": tested,
        "monotonic": monotonic,
    }


def run_warm_blocks(
    adapter,
    eval_queries: np.ndarray,
    eval_gt: np.ndarray,
    n_blocks: int,
    seed: int,
    out_dir: pathlib.Path,
    top_k_return: int = 100,
) -> dict:
    """Run `n_blocks` randomized-order warm evaluation blocks, one JSONL per block."""
    out_dir.mkdir(parents=True, exist_ok=True)
    n_queries = eval_queries.shape[0]
    block_files: list[pathlib.Path] = []
    block_p50_us: list[float] = []
    block_p95_us: list[float] = []
    block_p99_us: list[float] = []
    consumption_accumulator = 0
    first_block_results: np.ndarray | None = None

    for block_idx in range(n_blocks):
        rng = random.Random(seed + block_idx)
        order = list(range(n_queries))
        rng.shuffle(order)

        block_path = out_dir / f"block-{block_idx:02d}.jsonl"
        latencies_ns: list[int] = []
        block_results = np.empty((n_queries, top_k_return), dtype=np.int64)

        with open(block_path, "w") as f:
            for query_index in order:
                q = eval_queries[query_index]
                start = time.perf_counter_ns()
                ids = adapter.search_one(q, top_k_return)
                end = time.perf_counter_ns()
                latency_ns = end - start
                latencies_ns.append(latency_ns)
                block_results[query_index] = ids
                consumption_accumulator += int(ids[0])
                f.write(
                    json.dumps({"query_index": query_index, "latency_ns": latency_ns}) + "\n"
                )

        block_files.append(block_path)
        us = sorted(ns / 1000.0 for ns in latencies_ns)
        block_p50_us.append(_percentile(us, 0.50))
        block_p95_us.append(_percentile(us, 0.95))
        block_p99_us.append(_percentile(us, 0.99))
        if block_idx == 0:
            first_block_results = block_results

    return {
        "block_files": [str(p) for p in block_files],
        "block_p50_us": block_p50_us,
        "block_p95_us": block_p95_us,
        "block_p99_us": block_p99_us,
        "consumption_accumulator": consumption_accumulator,
        "first_block_results": first_block_results,
    }


def _percentile(sorted_values: list[float], q: float) -> float:
    if not sorted_values:
        return float("nan")
    idx = min(len(sorted_values) - 1, int(round(q * (len(sorted_values) - 1))))
    return sorted_values[idx]


def _coefficient_of_variation(values: list[float]) -> float:
    if len(values) < 2:
        return 0.0
    mean = statistics.mean(values)
    if mean == 0:
        return 0.0
    return (statistics.pstdev(values) / mean) * 100.0


def run_system(
    system_name: str,
    protocol: dict,
    base: np.ndarray,
    calib_queries: np.ndarray,
    calib_gt: np.ndarray,
    eval_queries: np.ndarray,
    eval_gt: np.ndarray,
    run_dir: pathlib.Path,
    run_id: str,
) -> dict:
    system_cfg = protocol["systems"][system_name]
    adapter_cls = ADAPTER_REGISTRY[system_name]
    kwargs = {}
    if system_name == "faiss-hnswflat":
        kwargs = {"m": system_cfg["m"], "ef_construction": system_cfg["ef_construction"]}
    elif system_name == "faiss-ivfflat":
        kwargs = {"nlist": system_cfg["nlist"], "train_sample": system_cfg["train_sample"]}
    elif system_name == "hnswlib":
        kwargs = {
            "m": system_cfg["m"],
            "ef_construction": system_cfg["ef_construction"],
            "seed": system_cfg["seed"],
        }

    system_dir = run_dir / system_name
    build_ms: list[float] = []
    peak_build_rss = 0
    adapter = None
    for i in range(protocol["execution"]["build_repeats"]):
        adapter = adapter_cls(**kwargs)
        build_out = system_dir / f"build-{i}"
        with RssSampler() as sampler:
            start = time.perf_counter()
            adapter.build(base, build_out)
            elapsed = time.perf_counter() - start
        build_ms.append(elapsed * 1000.0)
        peak_build_rss = max(peak_build_rss, sampler.peak_bytes)

    final_build_dir = system_dir / "build-final"
    adapter.save(final_build_dir)
    artifact_paths = adapter.artifact_paths(final_build_dir)
    from adapters.base import index_bytes

    calibration = calibrate_search_width(
        adapter,
        calib_queries,
        calib_gt,
        protocol["search_width"]["min_value"],
        system_cfg.get("search_width_max", protocol["search_width"]["max_value"]),
        protocol["recall"]["calibration_target"],
    )

    summary: dict = {
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "system": system_name,
        "cache_state": "warm",
        "query_count": 0,
        "cpu_affinity": None,
        "build_ms": build_ms,
        "build_ms_median": statistics.median(build_ms),
        "build_ms_cv_pct": _coefficient_of_variation(build_ms),
        "peak_build_rss_bytes": peak_build_rss,
        "index_bytes": index_bytes(artifact_paths),
        **adapter.metadata(),
    }

    if calibration["selected_width"] is None:
        summary["pass"] = False
        summary["notes"] = (
            f"failed calibration: recall@10={calibration['selected_recall']:.4f} "
            f"< target {protocol['recall']['calibration_target']} at max search width"
        )
        summary["calibration"] = calibration
        return summary

    adapter.set_search_width(calibration["selected_width"])
    summary["search_param_value"] = calibration["selected_width"]
    summary["calibration"] = calibration

    with RssSampler() as query_sampler:
        blocks = run_warm_blocks(
            adapter,
            eval_queries,
            eval_gt,
            protocol["execution"]["warm_blocks"],
            protocol["execution"]["block_order_seed"],
            system_dir / "blocks",
        )

    summary["peak_query_rss_bytes"] = query_sampler.peak_bytes
    summary["query_count"] = eval_queries.shape[0]
    summary["block_p50_us"] = blocks["block_p50_us"]
    summary["block_p95_us"] = blocks["block_p95_us"]
    summary["block_p99_us"] = blocks["block_p99_us"]
    summary["block_files"] = blocks["block_files"]
    summary["warm_p50_cv_pct"] = _coefficient_of_variation(blocks["block_p50_us"])
    summary["consumption_accumulator"] = blocks["consumption_accumulator"]

    first_results = blocks["first_block_results"]
    summary["recall_at_1"] = recall_at_k(first_results, eval_gt, 1)
    summary["recall_at_10"] = recall_at_k(first_results, eval_gt, 10)
    summary["recall_at_100"] = recall_at_k(first_results, eval_gt, 100)
    summary["pass"] = (
        protocol["recall"]["eval_band_low"]
        <= summary["recall_at_10"]
        <= protocol["recall"]["eval_band_high"]
    )
    return summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--protocol", required=True)
    parser.add_argument("--data", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument(
        "--systems",
        default="faiss-flat,faiss-hnswflat,faiss-ivfflat,hnswlib",
        help="comma-separated system names from protocol.toml [systems.*]",
    )
    parser.add_argument("--run-id", default=None)
    args = parser.parse_args(argv)

    with open(args.protocol, "rb") as f:
        protocol = tomllib.load(f)

    data_dir = pathlib.Path(args.data)
    ds_cfg = protocol["dataset"]
    manifest = ann_dataset.build_manifest(data_dir)
    failures = ann_dataset.validate_manifest(
        manifest,
        ds_cfg["expected_base_count"],
        ds_cfg["expected_query_count"],
        ds_cfg["expected_dim"],
        ds_cfg["expected_groundtruth_k"],
    )
    if failures:
        for msg in failures:
            print(f"dataset validation failed: {msg}", file=sys.stderr)
        return 2

    base, calib_q, calib_gt, eval_q, eval_gt = ann_dataset.load_split(
        data_dir,
        ds_cfg["split"]["calibration_end"],
        ds_cfg["split"]["evaluation_start"],
    )

    run_dir = pathlib.Path(args.out)
    run_dir.mkdir(parents=True, exist_ok=True)
    run_id = args.run_id or run_dir.name

    systems = [s.strip() for s in args.systems.split(",") if s.strip()]
    summaries = []
    for system_name in systems:
        if system_name not in ADAPTER_REGISTRY:
            print(
                f"skipping {system_name}: no adapter registered "
                f"(rust khive-vamana / diskann-memory are deferred, see adapters/FOLLOW-UP.md)",
                file=sys.stderr,
            )
            continue
        print(f"== {system_name} ==", file=sys.stderr)
        summary = run_system(
            system_name, protocol, base, calib_q, calib_gt, eval_q, eval_gt, run_dir, run_id
        )
        out_path = run_dir / f"{system_name}.summary.json"
        out_path.write_text(json.dumps(summary, indent=2, sort_keys=True, default=str) + "\n")
        summaries.append(summary)
        print(
            f"   pass={summary.get('pass')} "
            f"recall@10={summary.get('recall_at_10')} "
            f"width={summary.get('search_param_value')}",
            file=sys.stderr,
        )

    (run_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
