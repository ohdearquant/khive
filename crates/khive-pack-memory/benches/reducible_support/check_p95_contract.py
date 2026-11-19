#!/usr/bin/env python3
"""Check p95_gate listing/output independently of performance or model inference.

Pass an already-built p95_gate executable. --measure additionally runs its actual
cohort with a 300-second watchdog. Smoke, argument, lifecycle and report probes
exercise model-free paths, including with a real-model selector.
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


def parse_reports(text, expected, *, expected_statistic="p95_ns"):
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
        if row["statistic"] != expected_statistic:
            raise ValueError("expected report statistic " + expected_statistic)
        if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
            raise ValueError("expected finite positive timing")
        if type(count) is not int or count != 200:
            raise ValueError("expected all 200 measured samples")
        rows[name] = row
    if set(rows) != expected:
        raise ValueError("missing report ids: " + repr(sorted(expected - set(rows))))
    return rows


def probe(binary, args, environment, timeout, expected_code=0):
    result = subprocess.run(
        [str(binary)] + args,
        env=environment,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode != expected_code:
        raise ValueError(
            f"benchmark probe {args!r} exited {result.returncode}, expected {expected_code}: "
            + result.stderr + result.stdout
        )
    return result


def invoke(binary, args, environment, timeout):
    return probe(binary, args, environment, timeout).stdout


CASES = (
    ("held-rebuild", 0),
    ("held-build", 0),
    ("chained-handoff", 0),
    ("timeout-gating-row", 2),
    ("timeout-informational-row", 0),
    ("unrelated-task", 0),
    ("failed-task-completion", 0),
    ("clean-row", 0),
    ("unmeasurable-gating-row", 2),
)
SMOKE_COMPLETE = "p95 smoke: 9 cases passed"
CASE_PREFIX = "p95 gate regression case "
MEASUREMENT_MARKERS = (
    "BENCH-JSON ",
    "cohort: ",
    "memory.recall p95 baseline and regression gate",
    "memory.recall p95 regression gate — file-backed WAL, warm ANN path",
    "seeded ",
    "db: isolated file-backed WAL pool",
)


def require_no_measurement(result):
    combined = result.stdout + "\n" + result.stderr
    if any(marker in combined for marker in MEASUREMENT_MARKERS):
        raise ValueError("contract probe entered measurement output")


def check_case_witnesses(result, cases, smoke=False):
    require_no_measurement(result)
    expected = [f"{CASE_PREFIX}{name}: completed, exit {code}" for name, code in cases]
    witnessed = [line for line in result.stderr.splitlines() if line.startswith(CASE_PREFIX)]
    if sorted(witnessed) != sorted(expected):
        raise ValueError("missing, duplicate or incorrect lifecycle witnesses")
    if any(line.startswith(CASE_PREFIX) for line in result.stdout.splitlines()):
        raise ValueError("lifecycle witnesses must be on stderr")
    completions = result.stderr.splitlines().count(SMOKE_COMPLETE)
    if completions != int(smoke) or SMOKE_COMPLETE in result.stdout:
        raise ValueError("missing, duplicate or misplaced smoke completion")


def check_smoke(binary):
    # The invalid selector is intentional: the old harness refuses it before
    # any fixture, so the first old-head failure cannot load a real model.
    for selector in ("invalid", "0", "1"):
        environment = dict(os.environ)
        environment["KHIVE_BENCH_REAL_MODEL"] = selector
        for args in (
            ["--test"],
            ["--test", "--bench"],
            [],
            ["--save-baseline", "contract-probe", "--noplot"],
        ):
            result = probe(binary, args, environment, 15)
            check_case_witnesses(result, CASES, smoke=True)


def check_argument_refusals(binary):
    environment = dict(os.environ)
    environment["KHIVE_BENCH_REAL_MODEL"] = "invalid"
    for args in (
        ["--unknown"],
        ["unexpected-filter"],
        ["--regression-case", "clean-row"],
        ["--regression-case=unknown"],
        ["--save-baseline"],
        ["--list", "--unknown"],
        ["--list", "--regression-case", "clean-row"],
        ["--test", "--regression-case=clean-row"],
    ):
        result = probe(binary, args, environment, 5, expected_code=2)
        require_no_measurement(result)
        if "--regression-case=name" not in result.stderr:
            raise ValueError("argument refusal must name the accepted regression spelling")
        if CASE_PREFIX in result.stderr or SMOKE_COMPLETE in result.stderr:
            raise ValueError("invalid arguments reached lifecycle execution")


def check_lifecycle(binary):
    environment = dict(os.environ)
    environment["KHIVE_BENCH_REAL_MODEL"] = "0"
    for case, code in CASES:
        result = probe(binary, ["--regression-case=" + case], environment, 15, expected_code=code)
        check_case_witnesses(result, [(case, code)])


def check_caller_argv(binary):
    for real in (False, True):
        environment = dict(os.environ)
        environment["KHIVE_BENCH_REAL_MODEL"] = "1" if real else "0"
        cohort = "real-model" if real else "synthetic-v1"
        expected = {"p95_gate/" + cohort + "/" + case for case in ("m1", "m3", "m4-informational")}
        # bench-component.yml:98 supplies these flags; listing avoids measurement.
        listed = probe(binary, ["--list", "--quick", "--noplot"], environment, 5)
        require_no_measurement(listed)
        if parse_list(listed.stdout) != expected:
            raise ValueError("quick arguments change the listing")
        # bench-component.yml:98 flags must also preserve explicit smoke mode.
        smoke = probe(binary, ["--test", "--quick", "--noplot"], environment, 15)
        check_case_witnesses(smoke, CASES, smoke=True)
        # box_bench.py:423,506 supplies list and baseline arguments.
        listed = probe(binary, ["--list", "--save-baseline", "aa-x", "--noplot"], environment, 5)
        require_no_measurement(listed)
        if parse_list(listed.stdout) != expected:
            raise ValueError("baseline arguments change the listing")


def check(binary, measure=False):
    check_smoke(binary)
    check_argument_refusals(binary)
    check_caller_argv(binary)
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
        if parse_list(invoke(binary, ["--list", "--test", "--bench", "--regression-case=clean-row"], environment, 5)) != expected:
            raise ValueError("valid modes must not override listing")
        output = invoke(binary, ["--regression-case=report-contract", "--save-baseline", "contract-probe", "--noplot", "--bench"], environment, 5)
        rows = parse_reports(output, expected, expected_statistic="report_contract_probe")
        if any(row["value"] != 250_000.0 for row in rows.values()):
            raise ValueError("known report-contract values changed")
    check_lifecycle(binary)
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
