#!/usr/bin/env python3
"""Ingest a scale-proof bench JSON into the perf ledger CSV.

Usage:
    python3 scripts/perf/ingest_scale_proof.py \\
        --in    <bench-output>.json \\
        --ledger perf/ledger.csv

Appends ONE ROW PER N POINT from the bench JSON (not just the max-N row).
Creates perf/ledger.csv with a header row if the file does not exist.

CSV schema (one row per N):
    date,sha,target,n,beam,recall_at_10,p50_us,p95_us,p99_us,build_ms,speedup,pass,loadavg,notes

Stdlib-only: json, csv, sys, os, argparse, pathlib — no third-party deps.
"""

from __future__ import annotations

import argparse
import csv
import json
import pathlib
import sys

LEDGER_HEADER = [
    "date",
    "sha",
    "target",
    "n",
    "beam",
    "recall_at_10",
    "p50_us",
    "p95_us",
    "p99_us",
    "build_ms",
    "speedup",
    "brute_us",
    "pass",
    "loadavg",
    "notes",
]


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Ingest scale-proof bench JSON into perf ledger CSV (one row per N)."
    )
    p.add_argument("--in", dest="bench_json", required=True, help="Path to bench JSON file")
    p.add_argument(
        "--ledger",
        default="perf/ledger.csv",
        help="Path to ledger CSV (created if absent, default: perf/ledger.csv)",
    )
    p.add_argument(
        "--ledger-only",
        action="store_true",
        help="Append row even when assertions.overall != PASS and exit 0 (archival mode).",
    )
    return p.parse_args()


def ensure_ledger(ledger_path: pathlib.Path) -> None:
    if not ledger_path.exists():
        ledger_path.parent.mkdir(parents=True, exist_ok=True)
        with ledger_path.open("w", newline="") as f:
            writer = csv.writer(f, lineterminator="\n")
            writer.writerow(LEDGER_HEADER)
        print(f"Created new ledger: {ledger_path}", file=sys.stderr)


def build_notes(
    bench_row: dict,
    runner_os: str,
    dataset_name: str,
    machine_model: str,
    ram_bytes: int,
) -> str:
    parts = [dataset_name, runner_os]
    if machine_model and machine_model not in runner_os:
        parts.append(machine_model)
    if ram_bytes > 0:
        ram_gib = ram_bytes / (1024**3)
        parts.append(f"{ram_gib:.0f}GiB")
    return " ".join(parts)


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

    bench_rows = data.get("rows", [])
    if not bench_rows:
        print("ERROR: bench JSON has no rows", file=sys.stderr)
        sys.exit(1)

    produced_at = data.get("produced_at", "")
    git_sha = data.get("git_sha", "")
    runner_os = data.get("runner_os", "")
    machine_model = data.get("machine_model", "")
    ram_bytes = int(data.get("ram_bytes", 0))
    loadavg1 = float(data.get("loadavg1", 0.0))

    assertions = data.get("assertions", {})
    target_key = assertions.get("target_key", data.get("dataset", {}).get("name", ""))
    overall = assertions.get("overall", "SKIPPED")
    pass_str = "PASS" if overall == "PASS" else "FAIL"

    dataset_name = data.get("dataset", {}).get("name", "")

    # Sort rows by ascending N (bench writes them in order, but be defensive).
    bench_rows = sorted(bench_rows, key=lambda r: r.get("n", 0))

    ledger_path = pathlib.Path(args.ledger)
    ensure_ledger(ledger_path)

    appended = []
    with ledger_path.open("a", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LEDGER_HEADER, lineterminator="\n")
        for bench_row in bench_rows:
            n = bench_row.get("n", "")
            beam = bench_row.get("iso_recall_beam", "")
            recall = bench_row.get("recall_at_10", "")
            p50 = bench_row.get("query_warm_p50_us", "")
            p95 = bench_row.get("query_warm_p95_us", "")
            p99 = bench_row.get("query_warm_p99_us", "")
            build_ms = bench_row.get("build_ms", "")
            speedup = bench_row.get("speedup_vs_brute_force", "")
            brute_us = bench_row.get("bruteforce_p50_us", "")

            # Round floats to reasonable precision for readability.
            if isinstance(recall, float):
                recall = round(recall, 4)
            if isinstance(speedup, float):
                speedup = round(speedup, 1)
            if isinstance(brute_us, float):
                brute_us = round(brute_us, 3)
            if isinstance(build_ms, float):
                build_ms = round(build_ms, 3)
            if isinstance(p50, float):
                p50 = round(p50, 3)
            if isinstance(p95, float):
                p95 = round(p95, 3)
            if isinstance(p99, float):
                p99 = round(p99, 3)

            notes = build_notes(bench_row, runner_os, dataset_name, machine_model, ram_bytes)

            ledger_row = {
                "date": produced_at,
                "sha": git_sha[:7],
                "target": target_key,
                "n": n,
                "beam": beam,
                "recall_at_10": recall,
                "p50_us": p50,
                "p95_us": p95,
                "p99_us": p99,
                "build_ms": build_ms,
                "speedup": speedup,
                "brute_us": brute_us,
                "pass": pass_str,
                "loadavg": loadavg1,
                "notes": notes,
            }
            writer.writerow(ledger_row)
            appended.append(ledger_row)

    print(f"Appended {len(appended)} row(s) to {ledger_path}:")
    for r in appended:
        sha_short = str(r["sha"])[:8]
        print(
            f"  n={r['n']:>8}  beam={r['beam']:>4}  recall={r['recall_at_10']}  "
            f"p50={r['p50_us']}µs  p95={r['p95_us']}µs  brute_us={r['brute_us']}µs  "
            f"speedup={r['speedup']}x  {r['pass']}  sha={sha_short}"
        )

    if overall != "PASS" and not args.ledger_only:
        print(
            f"ERROR: assertions.overall={overall!r} — bench did not PASS. "
            "Use --ledger-only to append failed rows for archival.",
            file=sys.stderr,
        )
        sys.exit(1)


if __name__ == "__main__":
    main()
