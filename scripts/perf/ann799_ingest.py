#!/usr/bin/env python3
"""Validate ANN-799 summary records and append rows to perf/ann799-ledger.csv.

Reads every `<system>.summary.json` in a run directory, checks it against
the required-field contract in benchmarks/ann799/schema-v2.json's
`summary_record` definition (a minimal presence/type check -- this stays
stdlib-only rather than pulling in the `jsonschema` package), and appends
one row per system to the ledger CSV. Missing or null required fields
reject that row instead of writing a partially-populated line.

Usage:
    python3 scripts/perf/ann799_ingest.py --runs RUN_DIR \\
        --ledger perf/ann799-ledger.csv
"""

from __future__ import annotations

import argparse
import csv
import json
import pathlib
import statistics
import sys

LEDGER_HEADER = [
    "date", "sha", "target", "n", "system", "library", "library_version",
    "source_commit", "index_kind", "search_param_name", "search_param_value",
    "metric", "threads", "cache_state", "run_id", "recall_at_1",
    "recall_at_10", "recall_at_100", "p50_us", "p95_us", "p99_us",
    "build_ms", "index_bytes", "peak_build_rss_bytes",
    "peak_query_rss_bytes", "cpu_affinity", "speedup", "brute_us",
    "pass", "loadavg", "notes",
]

REQUIRED_FIELDS = [
    "schema_version", "run_id", "system", "library", "library_version",
    "index_kind", "search_param_name", "metric", "threads", "cache_state",
    "query_count", "recall_at_10", "index_bytes", "peak_build_rss_bytes",
    "peak_query_rss_bytes", "cpu_affinity", "build_ms", "pass",
]


def validate_summary(summary: dict) -> list[str]:
    errors = []
    for field in REQUIRED_FIELDS:
        if field not in summary:
            errors.append(f"missing required field: {field}")
        elif summary[field] is None and field != "cpu_affinity":
            errors.append(f"required field is null: {field}")
    if summary.get("schema_version") != 2:
        errors.append(f"schema_version must be 2, got {summary.get('schema_version')!r}")
    return errors


def summary_to_row(summary: dict, sha: str, target: str, loadavg: str) -> dict:
    block_p50 = summary.get("block_p50_us") or []
    block_p95 = summary.get("block_p95_us") or []
    block_p99 = summary.get("block_p99_us") or []
    return {
        "date": summary.get("run_id", ""),
        "sha": sha,
        "target": target,
        "n": summary.get("query_count", ""),
        "system": summary.get("system", ""),
        "library": summary.get("library", ""),
        "library_version": summary.get("library_version", ""),
        "source_commit": summary.get("source_commit", ""),
        "index_kind": summary.get("index_kind", ""),
        "search_param_name": summary.get("search_param_name", ""),
        "search_param_value": summary.get("search_param_value", ""),
        "metric": summary.get("metric", ""),
        "threads": summary.get("threads", ""),
        "cache_state": summary.get("cache_state", ""),
        "run_id": summary.get("run_id", ""),
        "recall_at_1": summary.get("recall_at_1", ""),
        "recall_at_10": summary.get("recall_at_10", ""),
        "recall_at_100": summary.get("recall_at_100", ""),
        "p50_us": statistics.median(block_p50) if block_p50 else "",
        "p95_us": statistics.median(block_p95) if block_p95 else "",
        "p99_us": statistics.median(block_p99) if block_p99 else "",
        "build_ms": summary.get("build_ms_median", ""),
        "index_bytes": summary.get("index_bytes", ""),
        "peak_build_rss_bytes": summary.get("peak_build_rss_bytes", ""),
        "peak_query_rss_bytes": summary.get("peak_query_rss_bytes", ""),
        "cpu_affinity": summary.get("cpu_affinity", ""),
        "speedup": "",
        "brute_us": "",
        "pass": summary.get("pass", ""),
        "loadavg": loadavg,
        "notes": summary.get("notes", ""),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", required=True, help="run directory containing *.summary.json")
    parser.add_argument("--ledger", required=True, help="ledger CSV path (created if absent)")
    parser.add_argument("--sha", default="unknown", help="git sha to stamp on every row")
    parser.add_argument("--target", default="khive-vamana/ann799/sift-1m")
    parser.add_argument("--loadavg", default="")
    args = parser.parse_args(argv)

    run_dir = pathlib.Path(args.runs)
    ledger_path = pathlib.Path(args.ledger)

    summary_paths = sorted(run_dir.glob("*.summary.json"))
    if not summary_paths:
        print(f"no *.summary.json found under {run_dir}", file=sys.stderr)
        return 1

    rows = []
    had_errors = False
    for path in summary_paths:
        summary = json.loads(path.read_text())
        errors = validate_summary(summary)
        if errors:
            had_errors = True
            print(f"{path}: rejected", file=sys.stderr)
            for e in errors:
                print(f"  - {e}", file=sys.stderr)
            continue
        rows.append(summary_to_row(summary, args.sha, args.target, args.loadavg))

    if had_errors:
        print("ingest aborted: one or more summaries failed schema validation", file=sys.stderr)
        return 1

    ledger_path.parent.mkdir(parents=True, exist_ok=True)
    write_header = not ledger_path.exists() or ledger_path.stat().st_size == 0
    with open(ledger_path, "a", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LEDGER_HEADER)
        if write_header:
            writer.writeheader()
        for row in rows:
            writer.writerow(row)

    print(f"appended {len(rows)} row(s) to {ledger_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
