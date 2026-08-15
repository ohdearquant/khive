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
    rows: dict[str, dict] = {}
    with path.open() as f:
        for lineno, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            d = json.loads(line)
            qid = d["query_id"]
            if qid in rows:
                raise ValueError(
                    f"{path}: duplicate query_id {qid!r} at line {lineno} — "
                    "already loaded from an earlier line in this file"
                )
            rows[qid] = d
    return rows


def paired_deltas(a: dict[str, dict], b: dict[str, dict], metric: str) -> list[float]:
    ids_a, ids_b = set(a), set(b)
    if ids_a != ids_b:
        detail = []
        only_a = sorted(ids_a - ids_b)
        only_b = sorted(ids_b - ids_a)
        if only_a:
            detail.append(f"in A but missing from B: {only_a}")
        if only_b:
            detail.append(f"in B but missing from A: {only_b}")
        raise ValueError(
            "paired comparison requires identical query_id sets between the "
            "two result files; " + "; ".join(detail)
        )
    if not ids_a:
        raise ValueError("no query_id rows in either result file")
    shared = sorted(ids_a)
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


def cohens_dz(deltas: list[float]) -> float | None:
    """Paired standardized effect size. Returns None ("undefined") when the
    sample standard deviation is zero: every delta is the same nonzero value,
    a deterministic uniform shift with no defined standardized size — not a
    zero effect, which 0.0 would misreport it as. Precondition: len(deltas)
    >= 2; compare() reports n<2 as insufficient sample separately and does
    not call this."""
    mean = sum(deltas) / len(deltas)
    sd = statistics.stdev(deltas)
    return mean / sd if sd > 0 else None


def cohens_dz_note(deltas: list[float], dz: float | None) -> str | None:
    if dz is not None:
        return None
    if len(deltas) < 2:
        return "insufficient sample (n<2)"
    return "undefined (zero variance)"


def compare(path_a: Path, path_b: Path, seed: int) -> list[dict]:
    a = load_rows(path_a)
    b = load_rows(path_b)
    out = []
    for metric in METRICS:
        deltas = paired_deltas(a, b, metric)
        rng = random.Random(seed)
        ci_lo, ci_hi = bootstrap_ci(deltas, rng, N_BOOTSTRAP)
        p = sign_flip_p(deltas, rng, N_SIGN_FLIP)
        dz = cohens_dz(deltas) if len(deltas) >= 2 else None
        out.append(
            {
                "metric": metric,
                "n_queries": len(deltas),
                "mean_delta": sum(deltas) / len(deltas),
                "ci95_lo": ci_lo,
                "ci95_hi": ci_hi,
                "p_sign_flip": p,
                "cohens_dz": dz,
                "cohens_dz_note": cohens_dz_note(deltas, dz),
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
        dz_display = (
            f"{r['cohens_dz']:.4f}"
            if r["cohens_dz"] is not None
            else r["cohens_dz_note"]
        )
        print(
            f"| {r['metric']} | {r['n_queries']} | {r['mean_delta']:.4f} | "
            f"[{r['ci95_lo']:.4f}, {r['ci95_hi']:.4f}] | {r['p_sign_flip']:.4f} | "
            f"{dz_display} |"
        )

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(rows, indent=2) + "\n")
        print(f"\nwrote {args.out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
