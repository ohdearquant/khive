#!/usr/bin/env python3
"""ANN-799 report generator: percentiles, bootstrap CI, CV, paired permutation test.

Reads the `<system>.summary.json` files and `block-NN.jsonl` raw-sample
files written by runner.py into one run directory, computes the statistics
required by docs/design/adr-799-baseline-plan.md (khive-work)'s
"Operating-point and statistical protocol" section, and renders the public
methodology-and-results markdown page.

Stdlib-only (random, statistics, math) so report generation never depends on
numpy/faiss/hnswlib being importable -- a report can be regenerated from
banked JSON/JSONL artifacts on a machine that never installed the ANN
libraries at all.

Usage:
    python3 benchmarks/ann799/report.py --runs "$ANN799_ROOT/run-<ts>" \\
        --out docs/benchmarks/ann799-matched-ann.md
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import random
import statistics


def load_run(run_dir: pathlib.Path) -> dict[str, dict]:
    summaries: dict[str, dict] = {}
    for path in sorted(run_dir.glob("*.summary.json")):
        system = path.stem.replace(".summary", "")
        summaries[system] = json.loads(path.read_text())
    return summaries


def bootstrap_ci(
    values: list[float], resamples: int, ci: float, seed: int
) -> tuple[float, float]:
    """Percentile-method bootstrap CI over `values` (e.g. the ten block p50s)."""
    if not values:
        return (float("nan"), float("nan"))
    rng = random.Random(seed)
    n = len(values)
    means = []
    for _ in range(resamples):
        sample = [values[rng.randrange(n)] for _ in range(n)]
        means.append(statistics.mean(sample))
    means.sort()
    alpha = (1.0 - ci) / 2.0
    lo_idx = max(0, int(alpha * resamples))
    hi_idx = min(resamples - 1, int((1.0 - alpha) * resamples))
    return (means[lo_idx], means[hi_idx])


def coefficient_of_variation(values: list[float]) -> float:
    if len(values) < 2:
        return 0.0
    mean = statistics.mean(values)
    if mean == 0:
        return 0.0
    return (statistics.pstdev(values) / mean) * 100.0


def paired_permutation_test(
    a: list[float], b: list[float], resamples: int, seed: int
) -> dict:
    """Two-sided paired permutation test over matched block p50 values.

    Returns p-value, Cohen's dz for the paired differences, and the
    bootstrap 95% CI of the paired median difference. `a` and `b` must be
    the same length (matched by block index).
    """
    if len(a) != len(b) or not a:
        return {"p_value": None, "cohens_dz": None, "median_diff_ci": None}

    diffs = [x - y for x, y in zip(a, b)]
    observed = abs(statistics.mean(diffs))

    rng = random.Random(seed)
    n = len(diffs)
    count_ge = 0
    for _ in range(resamples):
        signs = [rng.choice((1, -1)) for _ in range(n)]
        permuted = [d * s for d, s in zip(diffs, signs)]
        if abs(statistics.mean(permuted)) >= observed:
            count_ge += 1
    p_value = count_ge / resamples

    sd = statistics.pstdev(diffs)
    cohens_dz = (statistics.mean(diffs) / sd) if sd > 0 else float("inf")

    median_diffs = []
    for _ in range(resamples):
        sample = [diffs[rng.randrange(n)] for _ in range(n)]
        median_diffs.append(statistics.median(sample))
    median_diffs.sort()
    lo = median_diffs[int(0.025 * resamples)]
    hi = median_diffs[int(0.975 * resamples)]

    return {
        "p_value": p_value,
        "cohens_dz": cohens_dz,
        "median_diff_ci": [lo, hi],
        "median_diff": statistics.median(diffs),
    }


def block_percentiles_from_jsonl(block_path: pathlib.Path) -> dict:
    latencies_us = []
    with open(block_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            latencies_us.append(rec["latency_ns"] / 1000.0)
    latencies_us.sort()
    return {
        "p50_us": _percentile(latencies_us, 0.50),
        "p95_us": _percentile(latencies_us, 0.95),
        "p99_us": _percentile(latencies_us, 0.99),
        "n": len(latencies_us),
    }


def _percentile(sorted_values: list[float], q: float) -> float:
    if not sorted_values:
        return float("nan")
    idx = min(len(sorted_values) - 1, int(round(q * (len(sorted_values) - 1))))
    return sorted_values[idx]


def iso_recall_eligible(recall_at_10: float, band_low: float, band_high: float) -> bool:
    return band_low <= recall_at_10 <= band_high


def build_report(
    summaries: dict[str, dict],
    resamples: int = 10_000,
    ci: float = 0.95,
    seed: int = 799,
    band_low: float = 0.9500,
    band_high: float = 0.9600,
) -> dict:
    """Compute the aggregate statistics table used by the markdown renderer."""
    rows = {}
    for system, summary in summaries.items():
        block_p50 = summary.get("block_p50_us", [])
        rows[system] = {
            "system": system,
            "library": summary.get("library"),
            "recall_at_10": summary.get("recall_at_10"),
            "eligible": (
                iso_recall_eligible(summary["recall_at_10"], band_low, band_high)
                if summary.get("recall_at_10") is not None
                else False
            ),
            "p50_median_us": statistics.median(block_p50) if block_p50 else None,
            "p50_ci": bootstrap_ci(block_p50, resamples, ci, seed) if block_p50 else None,
            "p50_cv_pct": coefficient_of_variation(block_p50) if block_p50 else None,
            "build_ms_median": summary.get("build_ms_median"),
            "build_ms_cv_pct": summary.get("build_ms_cv_pct"),
            "index_bytes": summary.get("index_bytes"),
            "peak_build_rss_bytes": summary.get("peak_build_rss_bytes"),
            "peak_query_rss_bytes": summary.get("peak_query_rss_bytes"),
            "pass": summary.get("pass"),
            "search_param_name": summary.get("search_param_name"),
            "search_param_value": summary.get("search_param_value"),
        }

    comparisons = {}
    baseline = "khive-vamana"
    if baseline in summaries:
        for system, summary in summaries.items():
            if system == baseline:
                continue
            a = summaries[baseline].get("block_p50_us", [])
            b = summary.get("block_p50_us", [])
            if a and b and len(a) == len(b):
                comparisons[system] = paired_permutation_test(a, b, resamples, seed)

    return {"rows": rows, "comparisons": comparisons}


def render_markdown(report: dict, out_path: pathlib.Path) -> None:
    lines = ["<!-- RESULTS-PENDING: generated by report.py; regenerate after each run -->", ""]
    lines.append("## Iso-recall summary")
    lines.append("")
    lines.append(
        "| System | recall@10 | eligible (0.95-0.96) | warm p50 median (us) | p50 95% CI | p50 CV% | build ms median | build CV% |"
    )
    lines.append("| --- | --- | --- | --- | --- | --- | --- | --- |")
    for system, row in sorted(report["rows"].items()):
        ci_str = (
            f"[{row['p50_ci'][0]:.1f}, {row['p50_ci'][1]:.1f}]" if row["p50_ci"] else "n/a"
        )
        lines.append(
            f"| {system} | {row['recall_at_10']} | {row['eligible']} | "
            f"{row['p50_median_us']} | {ci_str} | {row['p50_cv_pct']} | "
            f"{row['build_ms_median']} | {row['build_ms_cv_pct']} |"
        )
    lines.append("")
    lines.append("## Paired permutation tests vs. khive-vamana (block p50)")
    lines.append("")
    if report["comparisons"]:
        lines.append("| System | p-value | Cohen's dz | median diff 95% CI | distinguishable? |")
        lines.append("| --- | --- | --- | --- | --- |")
        for system, comp in sorted(report["comparisons"].items()):
            p = comp["p_value"]
            dz = comp["cohens_dz"]
            distinguishable = (
                p is not None
                and p <= 0.05
                and dz is not None
                and abs(dz) >= 0.5
                and comp["median_diff_ci"] is not None
                and not (comp["median_diff_ci"][0] <= 0 <= comp["median_diff_ci"][1])
            )
            verdict = "yes" if distinguishable else "not distinguishable in this protocol"
            ci_str = f"[{comp['median_diff_ci'][0]:.2f}, {comp['median_diff_ci'][1]:.2f}]" if comp["median_diff_ci"] else "n/a"
            lines.append(f"| {system} | {p} | {dz} | {ci_str} | {verdict} |")
    else:
        lines.append("No khive-vamana summary present in this run; comparisons pending.")
    lines.append("")
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, "a") as f:
        f.write("\n".join(lines) + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", required=True, help="run directory containing *.summary.json")
    parser.add_argument("--out", required=True, help="markdown file to append results to")
    parser.add_argument("--resamples", type=int, default=10_000)
    parser.add_argument("--seed", type=int, default=799)
    args = parser.parse_args(argv)

    run_dir = pathlib.Path(args.runs)
    summaries = load_run(run_dir)
    if not summaries:
        print(f"no *.summary.json found under {run_dir}", flush=True)
        return 1

    report = build_report(summaries, resamples=args.resamples, seed=args.seed)
    render_markdown(report, pathlib.Path(args.out))
    print(f"appended results to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
