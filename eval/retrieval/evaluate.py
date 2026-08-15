#!/usr/bin/env python3
"""Retrieval regression-gate runner for the committed eval harness.

Seeds a fresh, isolated scratch khive database with the deterministic 400-note
corpus from generate_corpus.py, runs the 40 graded queries in queries.jsonl
through a named retrieval condition, and reports nDCG@10 / Recall@100 /
TargetRecall@100 / MRR@10, overall and per query_class.

Never touches a production database: HOME and KHIVE_DB are both redirected
into a fresh scratch directory for the duration of the run, and the script
refuses to start if an inherited KHIVE_DB already points at an existing file
outside that scratch directory.

Usage:
    uv run python evaluate.py --out results/A_fused_direct.jsonl
    uv run python evaluate.py --check-gold
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from collections import defaultdict
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import generate_corpus

HERE = Path(__file__).parent
DEFAULT_QUERIES = HERE / "queries.jsonl"
DEFAULT_GOLD = HERE / "gold" / "A_fused_direct.json"
SCRATCH_MARKER = "khive-eval-retrieval-"
CANDIDATE_POOL = 100

# Conditions table: name -> extra memory.recall args layered on the shared
# base (query/top_k/include_breakdown). New legs (sparse fusion, reranker)
# plug in here as additional named entries.
CONDITIONS = {
    "A_fused_direct": {},
}


def refuse_unsafe_db_env() -> None:
    env_db = os.environ.get("KHIVE_DB")
    if not env_db:
        return
    resolved = Path(env_db).expanduser()
    if resolved.exists() and SCRATCH_MARKER not in str(resolved):
        raise SystemExit(
            f"refusing to run: KHIVE_DB={resolved} already exists and is outside this "
            "harness's scratch directory. This harness never reads or writes a "
            "pre-existing or production database. Unset KHIVE_DB and re-run."
        )


def make_scratch(scratch_dir: str | None) -> Path:
    if scratch_dir:
        root = Path(scratch_dir)
        root.mkdir(parents=True, exist_ok=True)
    else:
        root = Path(tempfile.mkdtemp(prefix=SCRATCH_MARKER))
    (root / "home").mkdir(exist_ok=True)
    return root


def scratch_env(root: Path, db_path: Path) -> dict:
    env = dict(os.environ)
    for key in ("KHIVE_CONFIG", "KHIVE_SOCKET", "KHIVE_PID", "KHIVE_LOCK"):
        env.pop(key, None)
    env["HOME"] = str(root / "home")
    env["KHIVE_DB"] = str(db_path)
    return env


def run_kkernel(
    kkernel: str, args: list[str], env: dict, **kw
) -> subprocess.CompletedProcess:
    cmd = [kkernel, *args, "--log", "error"]
    result = subprocess.run(
        cmd, env=env, capture_output=True, text=True, check=False, **kw
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(cmd)}\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )
    return result


def seed_corpus(
    kkernel: str, env: dict, db_path: Path, root: Path, seed: int, epoch: str
) -> list[dict]:
    notes = generate_corpus.build_corpus(DEFAULT_QUERIES, seed, epoch)
    if len(notes) != 400:
        raise SystemExit(f"expected 400 notes, generator produced {len(notes)}")
    ops_path = root / "seed_ops.jsonl"
    save_path = root / "seed_save.jsonl"
    with ops_path.open("w") as f:
        for n in notes:
            op = {
                "tool": "memory.remember",
                "args": {
                    "content": n["content"],
                    "salience": n["salience"],
                    "decay_factor": n["decay_factor"],
                    "memory_type": n["memory_type"],
                    "tags": [f"k:{n['key']}"],
                },
            }
            f.write(json.dumps(op) + "\n")

    run_kkernel(
        kkernel,
        [
            "exec",
            "--ops-file",
            str(ops_path),
            "--save-file",
            str(save_path),
            "--db",
            str(db_path),
        ],
        env,
    )

    with save_path.open() as f:
        rows = [json.loads(line) for line in f if line.strip()]
    failed = [r for r in rows if not r.get("ok")]
    if failed:
        raise RuntimeError(f"{len(failed)} seed ops failed, e.g. {failed[0]}")
    if len(rows) != 400:
        raise RuntimeError(f"expected 400 seed rows, got {len(rows)}")

    return notes


def key_id_map(db_path: Path) -> dict[str, str]:
    conn = sqlite3.connect(str(db_path))
    try:
        cur = conn.execute(
            "SELECT id, properties FROM notes WHERE properties IS NOT NULL"
        )
        mapping: dict[str, str] = {}
        for note_id, props_json in cur.fetchall():
            props = json.loads(props_json)
            for tag in props.get("tags", []):
                if tag.startswith("k:"):
                    mapping[tag[2:]] = note_id
        return mapping
    finally:
        conn.close()


def set_ages(db_path: Path, notes: list[dict], key_to_id: dict[str, str]) -> None:
    conn = sqlite3.connect(str(db_path))
    try:
        for n in notes:
            note_id = key_to_id[n["key"]]
            dt = datetime.fromisoformat(n["created_at_iso"].replace("Z", "+00:00"))
            micros = int(dt.timestamp() * 1_000_000)
            conn.execute(
                "UPDATE notes SET created_at = ?, updated_at = ? WHERE id = ?",
                (micros, micros, note_id),
            )
        conn.commit()
    finally:
        conn.close()


def run_condition(
    kkernel: str,
    env: dict,
    db_path: Path,
    root: Path,
    condition: str,
    queries: list[dict],
) -> list[dict]:
    extra_args = CONDITIONS[condition]
    ops_path = root / f"query_ops_{condition}.jsonl"
    save_path = root / f"query_save_{condition}.jsonl"
    with ops_path.open("w") as f:
        for q in queries:
            args = {
                "query": q["query"],
                "top_k": CANDIDATE_POOL,
                "include_breakdown": True,
                **extra_args,
            }
            f.write(json.dumps({"tool": "memory.recall", "args": args}) + "\n")

    run_kkernel(
        kkernel,
        [
            "exec",
            "--ops-file",
            str(ops_path),
            "--save-file",
            str(save_path),
            "--db",
            str(db_path),
        ],
        env,
    )

    with save_path.open() as f:
        rows = [json.loads(line) for line in f if line.strip()]
    if len(rows) != len(queries):
        raise RuntimeError(f"expected {len(queries)} query rows, got {len(rows)}")
    return rows


def dcg_at_k(grades: list[int], k: int) -> float:
    total = 0.0
    for i, g in enumerate(grades[:k], start=1):
        gain = (2**g) - 1
        total += gain / math.log2(i + 1)
    return total


def compute_metrics(query: dict, id_to_key: dict[str, str], result_row: dict) -> dict:
    labels = query["labels"]
    hits = result_row["result"]["results"]
    hit_ids = [h["id"] for h in hits]
    hit_grades = [labels.get(id_to_key.get(hid, ""), 0) for hid in hit_ids]

    ideal_grades = sorted(labels.values(), reverse=True)
    idcg10 = dcg_at_k(ideal_grades, 10)
    dcg10 = dcg_at_k(hit_grades, 10)
    ndcg10 = (dcg10 / idcg10) if idcg10 > 0 else 0.0

    total_relevant = sum(1 for g in labels.values() if g >= 1)
    retrieved_relevant = sum(1 for g in hit_grades if g >= 1)
    recall100 = (retrieved_relevant / total_relevant) if total_relevant > 0 else 0.0

    total_targets = sum(1 for g in labels.values() if g >= 2)
    retrieved_targets = sum(1 for g in hit_grades if g >= 2)
    target_recall100 = (retrieved_targets / total_targets) if total_targets > 0 else 0.0

    mrr10 = 0.0
    for i, g in enumerate(hit_grades[:10], start=1):
        if g >= 2:
            mrr10 = 1.0 / i
            break

    return {
        "query_id": query["query_id"],
        "cluster": query["cluster"],
        "query_class": query["query_class"],
        "candidate_count": len(hits),
        "nDCG@10": ndcg10,
        "Recall@100": recall100,
        "TargetRecall@100": target_recall100,
        "MRR@10": mrr10,
    }


def aggregate(rows: list[dict]) -> dict:
    metrics = ["nDCG@10", "Recall@100", "TargetRecall@100", "MRR@10"]

    def mean(vals: list[float]) -> float:
        return sum(vals) / len(vals) if vals else 0.0

    overall = {m: mean([r[m] for r in rows]) for m in metrics}
    by_class: dict[str, dict] = {}
    grouped = defaultdict(list)
    for r in rows:
        grouped[r["query_class"]].append(r)
    for cls, cls_rows in sorted(grouped.items()):
        by_class[cls] = {m: mean([r[m] for r in cls_rows]) for m in metrics}
    return {"overall": overall, "by_class": by_class}


def print_table(condition: str, agg: dict) -> None:
    metrics = ["nDCG@10", "Recall@100", "TargetRecall@100", "MRR@10"]
    print(f"\n== {condition} — overall ==")
    print("| Metric | Value |")
    print("| --- | --- |")
    for m in metrics:
        print(f"| {m} | {agg['overall'][m]:.4f} |")
    print(f"\n== {condition} — per query_class ==")
    print("| query_class | " + " | ".join(metrics) + " |")
    print("| --- | " + " | ".join("---" for _ in metrics) + " |")
    for cls, vals in agg["by_class"].items():
        print(f"| {cls} | " + " | ".join(f"{vals[m]:.4f}" for m in metrics) + " |")


def compare_gold(agg: dict, gold: dict, tol: float) -> list[str]:
    mismatches = []
    for m, v in agg["overall"].items():
        gv = gold["overall"][m]
        if abs(v - gv) > tol:
            mismatches.append(f"overall.{m}: got {v!r} vs gold {gv!r}")
    for cls, vals in agg["by_class"].items():
        for m, v in vals.items():
            gv = gold["by_class"][cls][m]
            if abs(v - gv) > tol:
                mismatches.append(f"{cls}.{m}: got {v!r} vs gold {gv!r}")
    return mismatches


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--kkernel", default="kkernel")
    ap.add_argument("--queries", type=Path, default=DEFAULT_QUERIES)
    ap.add_argument("--condition", default="A_fused_direct", choices=sorted(CONDITIONS))
    ap.add_argument(
        "--out", type=Path, default=None, help="per-query result JSONL output path"
    )
    ap.add_argument("--seed", type=int, default=generate_corpus.DEFAULT_SEED)
    ap.add_argument("--epoch", type=str, default=generate_corpus.DEFAULT_EPOCH)
    ap.add_argument("--scratch-dir", default=None)
    ap.add_argument("--keep-scratch", action="store_true")
    ap.add_argument("--check-gold", action="store_true")
    ap.add_argument("--gold", type=Path, default=DEFAULT_GOLD)
    ap.add_argument("--gold-tolerance", type=float, default=0.0)
    args = ap.parse_args()

    refuse_unsafe_db_env()
    root = make_scratch(args.scratch_dir)
    db_path = root / "eval.db"
    env = scratch_env(root, db_path)

    try:
        run_kkernel(args.kkernel, ["db", "migrate", "--db", str(db_path)], env)
        queries = generate_corpus.parse_queries(args.queries)
        notes = seed_corpus(args.kkernel, env, db_path, root, args.seed, args.epoch)
        key_to_id = key_id_map(db_path)
        missing = [n["key"] for n in notes if n["key"] not in key_to_id]
        if missing:
            raise RuntimeError(
                f"{len(missing)} seeded notes missing tag-based id, e.g. {missing[:5]}"
            )
        set_ages(db_path, notes, key_to_id)
        id_to_key = {v: k for k, v in key_to_id.items()}

        result_rows = run_condition(
            args.kkernel, env, db_path, root, args.condition, queries
        )
        per_query = [
            compute_metrics(q, id_to_key, row) for q, row in zip(queries, result_rows)
        ]
    finally:
        if not args.keep_scratch:
            shutil.rmtree(root, ignore_errors=True)

    agg = aggregate(per_query)
    print_table(args.condition, agg)

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("w") as f:
            for r in per_query:
                f.write(json.dumps({"condition": args.condition, **r}) + "\n")
        print(f"\nwrote {len(per_query)} rows to {args.out}")

    if args.check_gold:
        if not args.gold.exists():
            print(f"\ngold file not found: {args.gold}", file=sys.stderr)
            return 2
        gold = json.loads(args.gold.read_text())
        mismatches = compare_gold(agg, gold, args.gold_tolerance)
        if mismatches:
            print(
                f"\nGOLD CHECK FAILED ({len(mismatches)} mismatches):", file=sys.stderr
            )
            for m in mismatches:
                print(f"  {m}", file=sys.stderr)
            return 1
        print(
            "\nGOLD CHECK PASSED — matches gold/A_fused_direct.json within tolerance "
            f"{args.gold_tolerance}"
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
