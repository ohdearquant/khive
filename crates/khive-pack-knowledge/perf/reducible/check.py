"""Validate executable benchmark enumeration/report contracts, without building them."""

import argparse
import json
import math
import os
import subprocess

SEARCH_IDS = {
    "knowledge_search_latency/synthetic_100/" + name
    for name in ("rerank_false", "rerank_true", "default")
}
ARMS = (
    "one_index_write",
    "knowledge_delete_atoms_only",
    "m_rewrites_one_subject",
    "m_distinct_subjects",
    "vector_delete_then_atom_delete_two_commits",
)


def tail_ids(sizes=(10000,), m=16):
    return {
        f"knowledge_tail_replay/synthetic/n_{n}/m_{m}/hz_10/{arm}/first_search"
        for n in sizes
        for arm in ARMS
    }


def check_list(output, expected):
    lines = output.splitlines()
    ids = [line.removesuffix(": benchmark") for line in lines]
    if (
        not lines
        or any(not line.endswith(": benchmark") for line in lines)
        or len(ids) != len(set(ids))
        or set(ids) != expected
    ):
        raise ValueError(
            "listing must contain exactly the expected benchmark ids, once each"
        )


def check_report(output, expected):
    rows = [
        json.loads(line[len("BENCH-JSON ") :])
        for line in output.splitlines()
        if line.startswith("BENCH-JSON ")
    ]
    ids = []
    for row in rows:
        if not isinstance(row, dict) or set(row) != {"id", "statistic", "value", "n"}:
            raise ValueError("invalid report fields")
        if (
            row["statistic"] != "median_ns"
            or type(row["n"]) is not int
            or row["n"] != 1
        ):
            raise ValueError(
                "tail cases require one observation, reported as median_ns with n=1"
            )
        value = row["value"]
        if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
            raise ValueError("invalid sample value")
        if not isinstance(row["id"], str):
            raise ValueError("id must be a string")
        ids.append(row["id"])
    if len(ids) != len(set(ids)) or set(ids) != expected:
        raise ValueError("missing, duplicate or unexpected benchmark report id")


def invoke(binary, args, *, overrides=None, timeout=15):
    env = os.environ.copy()
    for key in (
        "KHIVE_BENCH_REAL_MODEL",
        "KHIVE_BENCH_TAIL_100K",
        "KHIVE_BENCH_TAIL_500K",
        "KHIVE_BENCH_TAIL_M",
        "KHIVE_BENCH_TAIL_WRITE_HZ",
    ):
        env.pop(key, None)
    env.update(overrides or {})
    result = subprocess.run(
        [str(binary), *args],
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=True,
    )
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--search", required=True, help="built search_latency executable"
    )
    parser.add_argument("--tail", required=True, help="built tail_replay executable")
    parser.add_argument(
        "--measure-tail",
        action="store_true",
        help="also measure the default 10k cohort, with a 300-second timeout",
    )
    args = parser.parse_args()
    listing = [
        "--list",
        "--regression-case=list-only",
        "--save-baseline",
        "protocol",
        "--noplot",
        "--bench",
    ]
    check_list(invoke(args.search, listing), SEARCH_IDS)
    check_list(
        invoke(args.search, listing, overrides={"KHIVE_BENCH_REAL_MODEL": "1"}),
        {id_.replace("synthetic_100", "real_model_100") for id_ in SEARCH_IDS},
    )
    check_list(invoke(args.tail, listing), tail_ids())
    check_list(invoke(args.tail, [*listing, "--quick"]), tail_ids((64,), 4))
    check_list(
        invoke(
            args.tail,
            listing,
            overrides={
                "KHIVE_BENCH_TAIL_100K": "1",
                "KHIVE_BENCH_TAIL_500K": "1",
            },
        ),
        tail_ids((10000, 100000, 500000)),
    )
    check_report(invoke(args.tail, ["--regression-case=report-contract"]), tail_ids())
    if args.measure_tail:
        check_report(
            invoke(args.tail, ["--bench", "--noplot"], timeout=300), tail_ids()
        )
    print(
        "PASS executable list/report contracts; protocol-control values are not measurements"
    )


if __name__ == "__main__":
    main()
