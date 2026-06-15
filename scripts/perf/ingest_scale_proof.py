#!/usr/bin/env python3
"""Ingest a scale-proof bench JSON into the perf ledger CSV.

Usage:
    uv run scripts/perf/ingest_scale_proof.py \\
        --in    probe-results-1m.json \\
        --ledger perf/ledger.csv

The script appends ONE row per run (the max-N row from the bench JSON).
It creates perf/ledger.csv with a header row if the file does not exist.

CSV schema:
    date,sha,pr,target,p50_us,p95_us,p99_us,max_us,pass,runner_os,loadavg1,notes

This script is separate from any Criterion ingest.py; it reads the
canonical scale-proof JSON schema (schema_version = "1.0").

Stdlib-only: json, csv, sys, os, argparse, pathlib — no third-party deps.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import pathlib
import sys

LEDGER_HEADER = [
    "date",
    "sha",
    "pr",
    "target",
    "p50_us",
    "p95_us",
    "p99_us",
    "max_us",
    "pass",
    "runner_os",
    "loadavg1",
    "notes",
]


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Ingest scale-proof bench JSON into perf ledger CSV.")
    p.add_argument("--in", dest="bench_json", required=True, help="Path to bench JSON file")
    p.add_argument(
        "--ledger",
        default="perf/ledger.csv",
        help="Path to ledger CSV (created if absent, default: perf/ledger.csv)",
    )
    return p.parse_args()


def ensure_ledger(ledger_path: pathlib.Path) -> None:
    if not ledger_path.exists():
        ledger_path.parent.mkdir(parents=True, exist_ok=True)
        with ledger_path.open("w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(LEDGER_HEADER)
        print(f"Created new ledger: {ledger_path}", file=sys.stderr)


def build_notes(max_row: dict, loadavg1: float, all_checks_pass: bool) -> str:
    n = max_row["n"]
    recall = max_row.get("recall_at_10", float("nan"))
    speedup = max_row.get("speedup_vs_brute_force", float("nan"))
    notes = f"n={n},recall={recall:.4f},speedup={speedup:.1f}x"
    if loadavg1 > 4.0:
        notes += ",high_load"
    if not all_checks_pass:
        # Check if any check is a latency-related fail.
        notes += ",assertion_fail"
    return notes


def main() -> None:
    args = parse_args()

    bench_path = pathlib.Path(args.bench_json)
    if not bench_path.exists():
        print(f"ERROR: bench JSON not found: {bench_path}", file=sys.stderr)
        sys.exit(1)

    with bench_path.open() as f:
        data = json.load(f)

    schema_version = data.get("schema_version")
    if schema_version != "1.0":
        print(
            f"WARNING: schema_version is '{schema_version}', expected '1.0'. Proceeding anyway.",
            file=sys.stderr,
        )

    rows = data.get("rows", [])
    if not rows:
        print("ERROR: bench JSON has no rows", file=sys.stderr)
        sys.exit(1)

    # Max-N row is the last element (bench writes rows in ascending N order).
    max_row = max(rows, key=lambda r: r.get("n", 0))

    produced_at = data.get("produced_at", "")
    git_sha = data.get("git_sha", "")
    runner_os = data.get("runner_os", "")
    loadavg1 = float(data.get("loadavg1", 0.0))
    pr = os.environ.get("GITHUB_PR_NUMBER", "")

    assertions = data.get("assertions", {})
    target_key = assertions.get("target_key", data.get("dataset", {}).get("name", ""))
    overall = assertions.get("overall", "SKIPPED")
    pass_str = "true" if overall == "PASS" else "false"

    checks = assertions.get("checks", [])
    all_checks_pass = all(c.get("result") == "PASS" for c in checks) if checks else (overall == "SKIPPED")

    notes = build_notes(max_row, loadavg1, all_checks_pass)

    row = {
        "date": produced_at,
        "sha": git_sha,
        "pr": pr,
        "target": target_key,
        "p50_us": max_row.get("query_warm_p50_us", ""),
        "p95_us": max_row.get("query_warm_p95_us", ""),
        "p99_us": max_row.get("query_warm_p99_us", ""),
        "max_us": max_row.get("query_warm_max_us", ""),
        "pass": pass_str,
        "runner_os": runner_os,
        "loadavg1": loadavg1,
        "notes": notes,
    }

    ledger_path = pathlib.Path(args.ledger)
    ensure_ledger(ledger_path)

    with ledger_path.open("a", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LEDGER_HEADER)
        writer.writerow(row)

    print(f"Appended row to {ledger_path}:")
    print(
        f"  date={produced_at}  sha={git_sha[:8]}...  target={target_key}  "
        f"p50={row['p50_us']}  p95={row['p95_us']}  p99={row['p99_us']}  "
        f"max={row['max_us']}  pass={pass_str}  notes={notes}"
    )


if __name__ == "__main__":
    main()
