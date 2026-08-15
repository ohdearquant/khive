#!/usr/bin/env python3
"""Paired bootstrap comparison between two evaluate.py result JSONLs.

For each shared metric, computes the paired mean difference (condition B
minus condition A), a 10,000-resample percentile bootstrap 95% CI, a
50,000-draw two-sided sign-flip permutation p-value, and Cohen's dz (the
paired standardized effect size). Deterministic given --seed — no wall clock
or unseeded randomness.

Usage:
    uv run python bootstrap.py results/A_fused_direct.jsonl results/B_other.jsonl
"""

from __future__ import annotations

import argparse
import json
import random
import statistics
from pathlib import Path

METRICS = ["nDCG@10", "Recall@100", "TargetRecall@100", "MRR@10"]
N_BOOTSTRAP = 10_000
N_SIGN_FLIP = 50_000


def load_rows(path: Path) -> dict[str, dict]:
    rows = {}
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            d = json.loads(line)
            rows[d["query_id"]] = d
    return rows


def paired_deltas(a: dict[str, dict], b: dict[str, dict], metric: str) -> list[float]:
    shared = sorted(set(a) & set(b))
    if not shared:
        raise ValueError("no shared query_id between the two result files")
    return [b[q][metric] - a[q][metric] for q in shared]


def bootstrap_ci(
    deltas: list[float], rng: random.Random, n: int
) -> tuple[float, float]:
    n_q = len(deltas)
    means = []
    for _ in range(n):
        sample = [deltas[rng.randrange(n_q)] for _ in range(n_q)]
        means.append(sum(sample) / n_q)
    means.sort()
    lo = means[int(0.025 * n)]
    hi = means[min(int(0.975 * n), n - 1)]
    return lo, hi


def sign_flip_p(deltas: list[float], rng: random.Random, n: int) -> float:
    observed = abs(sum(deltas) / len(deltas))
    count = 0
    for _ in range(n):
        flipped = sum(d if rng.random() < 0.5 else -d for d in deltas)
        if abs(flipped / len(deltas)) >= observed:
            count += 1
    return count / n


def cohens_dz(deltas: list[float]) -> float:
    if len(deltas) < 2:
        return 0.0
    mean = sum(deltas) / len(deltas)
    sd = statistics.stdev(deltas)
    return mean / sd if sd > 0 else 0.0


def compare(path_a: Path, path_b: Path, seed: int) -> list[dict]:
    a = load_rows(path_a)
    b = load_rows(path_b)
    out = []
    for metric in METRICS:
        deltas = paired_deltas(a, b, metric)
        rng = random.Random(seed)
        ci_lo, ci_hi = bootstrap_ci(deltas, rng, N_BOOTSTRAP)
        p = sign_flip_p(deltas, rng, N_SIGN_FLIP)
        dz = cohens_dz(deltas)
        out.append(
            {
                "metric": metric,
                "n_queries": len(deltas),
                "mean_delta": sum(deltas) / len(deltas),
                "ci95_lo": ci_lo,
                "ci95_hi": ci_hi,
                "p_sign_flip": p,
                "cohens_dz": dz,
            }
        )
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("result_a", type=Path, help="baseline condition's result JSONL")
    ap.add_argument("result_b", type=Path, help="comparison condition's result JSONL")
    ap.add_argument("--seed", type=int, default=939)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    rows = compare(args.result_a, args.result_b, args.seed)

    print(f"Paired comparison: {args.result_b.name} vs {args.result_a.name}")
    print("| Metric | n | Δ mean | 95% CI | p (sign-flip) | Cohen's dz |")
    print("| --- | --- | --- | --- | --- | --- |")
    for r in rows:
        print(
            f"| {r['metric']} | {r['n_queries']} | {r['mean_delta']:.4f} | "
            f"[{r['ci95_lo']:.4f}, {r['ci95_hi']:.4f}] | {r['p_sign_flip']:.4f} | "
            f"{r['cohens_dz']:.4f} |"
        )

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(rows, indent=2) + "\n")
        print(f"\nwrote {args.out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
