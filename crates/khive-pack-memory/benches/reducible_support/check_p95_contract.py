#!/usr/bin/env python3
"""Check p95_gate listing/output independently of performance or model inference.

Pass an already-built p95_gate executable. --measure additionally runs its actual
cohort with a 300-second watchdog; contract probes alone never load a model.
"""
import argparse
import json
import math
import os
from pathlib import Path
import subprocess


def parse_list(text):
    ids = []
    for line in text.splitlines():
        if not line.strip():
            continue
        if not line.endswith(": benchmark"):
            raise ValueError("unexpected listing line: " + line)
        ids.append(line[: -len(": benchmark")])
    if not ids or len(ids) != len(set(ids)):
        raise ValueError("listing must be nonempty with unique ids")
    return set(ids)


def parse_reports(text, expected):
    rows = {}
    for line in text.splitlines():
        if not line.startswith("BENCH-JSON "):
            continue
        row = json.loads(line[len("BENCH-JSON ") :])
        if not isinstance(row, dict) or set(row) != {"id", "statistic", "value", "n"}:
            raise ValueError("unexpected report fields")
        name = row["id"]
        if not isinstance(name, str) or name not in expected or name in rows:
            raise ValueError("unknown or duplicate report id")
        value, count = row["value"], row["n"]
        if row["statistic"] != "p95_ns":
            raise ValueError("p95 must report nanoseconds")
        if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
            raise ValueError("expected finite positive timing")
        if type(count) is not int or count != 200:
            raise ValueError("expected all 200 measured samples")
        rows[name] = row
    if set(rows) != expected:
        raise ValueError("missing report ids: " + repr(sorted(expected - set(rows))))
    return rows


def invoke(binary, args, environment, timeout):
    result = subprocess.run(
        [str(binary)] + args,
        env=environment,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode:
        raise ValueError("benchmark probe failed: " + result.stderr + result.stdout)
    return result.stdout


def check(binary, measure=False):
    for real in (False, True):
        environment = dict(os.environ)
        environment["KHIVE_BENCH_REAL_MODEL"] = "1" if real else "0"
        cohort = "real-model" if real else "synthetic-v1"
        expected = {"p95_gate/" + cohort + "/" + case for case in ("m1", "m3", "m4-informational")}
        listed = parse_list(invoke(binary, ["--list", "--regression-case=list-only"], environment, 5))
        if listed != expected:
            raise ValueError("listing differs from declared cohort")
        if parse_list(invoke(binary, ["--list", "--save-baseline", "contract-probe", "--noplot", "--bench"], environment, 5)) != expected:
            raise ValueError("compatibility arguments change the listing")
        output = invoke(binary, ["--regression-case=report-contract", "--save-baseline", "contract-probe", "--noplot", "--bench"], environment, 5)
        rows = parse_reports(output, expected)
        if any(row["value"] != 250_000.0 for row in rows.values()):
            raise ValueError("known report-contract values changed")
    if measure:
        # Run only the caller-selected cohort; real inference is never implicit.
        real = os.environ.get("KHIVE_BENCH_REAL_MODEL", "0") == "1"
        cohort = "real-model" if real else "synthetic-v1"
        expected = {"p95_gate/" + cohort + "/" + case for case in ("m1", "m3", "m4-informational")}
        parse_reports(invoke(binary, ["--bench"], dict(os.environ), 300), expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--measure", action="store_true")
    args = parser.parse_args()
    check(args.binary.resolve(strict=True), args.measure)
    print("p95 contract checks passed")


if __name__ == "__main__":
    main()
