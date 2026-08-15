#!/usr/bin/env python3
"""Offline SPLADE sparse-retrieval fusion experiment (R1 slice 2).

Adds an experimental, clearly out-of-band condition to the retrieval eval
harness: a SPLADE-v3-family sparse leg, computed entirely offline, fused with
the committed `A_fused_direct` condition's own ranking via the same RRF rule
the runtime uses (`DEFAULT_RRF_K = 60`, crates/khive-fusion/src/strategy.rs:7;
`rrf_score`, crates/khive-score/src/ops.rs:145 — `1 / (k + rank)`, 1-based
rank).

This is a two-stage *offline* approximation of a native three-leg fusion:
`A_fused_direct` is itself already an RRF fusion of the runtime's dense +
lexical legs, and this script RRF-fuses that already-fused ranking against a
third, independently-ranked SPLADE leg. A native in-runtime fusion would
combine all three legs' ranks in one RRF pass; running it as two sequential
RRF passes changes the effective rank distribution feeding the second pass
(a document's rank in the first-pass fusion is not the same statistic as its
rank in either individual leg), which is a source of bias — see README.md
"Methodology honesty" for the direction argued.

Two modes:

    --encode    Requires `transformers` + `torch` (install via
                `uv run --with transformers --with torch python
                splade_offline.py --encode ...`). Encodes the harness's 400
                corpus notes and 40 queries into sparse term-weight vectors
                and writes them to a local JSON fixture (~1MB — not
                committed; this repo's pre-commit large-file guard caps at
                512KB, and the fixture regenerates in ~17s on a warm model
                cache, so shipping it isn't worth the size). Never touches
                `kkernel` or the eval harness's scratch/gold machinery.

    (default)   Requires only `kkernel` on PATH and a fixture written by
                --encode. Seeds the harness's standard scratch corpus, runs
                the committed `A_fused_direct` condition to get its raw
                per-query ranking, computes the SPLADE leg's ranking from the
                fixture, RRF-fuses the two, scores the fused ranking with the
                harness's own nDCG@10/Recall@100/TargetRecall@100/MRR@10
                metrics, and writes a `B_splade_offline` results JSONL in the
                same shape `evaluate.py --out` writes (so `bootstrap.py`
                compares directly against `results/A_fused_direct.jsonl`).

Never modifies `evaluate.py`'s CONDITIONS table, `gold/A_fused_direct.json`,
or any existing condition — this is a fully separate, additive script.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import evaluate
import generate_corpus

HERE = Path(__file__).parent
DEFAULT_QUERIES = HERE / "queries.jsonl"
RRF_K = 60  # crates/khive-fusion/src/strategy.rs:7 DEFAULT_RRF_K
SPLADE_POOL = 100  # matches evaluate.CANDIDATE_POOL so both legs contribute equal-depth pools


def _sparse_encode_batch(model, tok, texts: list[str], device, max_length: int):
    import torch

    enc = tok(
        texts,
        padding=True,
        truncation=True,
        max_length=max_length,
        return_tensors="pt",
    ).to(device)
    with torch.no_grad():
        logits = model(**enc).logits  # (batch, seq, vocab)
        relu = torch.relu(logits)
        weighted = torch.log1p(relu)
        mask = enc["attention_mask"].unsqueeze(-1)  # (batch, seq, 1)
        weighted = weighted * mask
        pooled, _ = torch.max(weighted, dim=1)  # (batch, vocab) — SpladePooling max strategy
    return pooled.cpu()


def encode_all(model_name: str, max_length: int, batch_size: int) -> dict:
    from transformers import AutoModelForMaskedLM, AutoTokenizer

    tok = AutoTokenizer.from_pretrained(model_name)
    model = AutoModelForMaskedLM.from_pretrained(model_name)
    model.eval()
    device = "cpu"
    model.to(device)
    id_to_token = {v: k for k, v in tok.get_vocab().items()}

    notes = generate_corpus.build_corpus(
        DEFAULT_QUERIES, generate_corpus.DEFAULT_SEED, generate_corpus.DEFAULT_EPOCH
    )
    queries = generate_corpus.parse_queries(DEFAULT_QUERIES)

    def sparse_dict(vec) -> dict[str, float]:
        nz = (vec > 1e-4).nonzero(as_tuple=True)[0].tolist()
        return {id_to_token[i]: round(float(vec[i]), 5) for i in nz}

    doc_vectors: dict[str, dict[str, float]] = {}
    doc_times: list[float] = []
    texts = [n["content"] for n in notes]
    keys = [n["key"] for n in notes]
    for start in range(0, len(texts), batch_size):
        batch_texts = texts[start : start + batch_size]
        batch_keys = keys[start : start + batch_size]
        t0 = time.perf_counter()
        pooled = _sparse_encode_batch(model, tok, batch_texts, device, max_length)
        elapsed = time.perf_counter() - t0
        per_doc = elapsed / len(batch_texts)
        for key, vec in zip(batch_keys, pooled):
            doc_vectors[key] = sparse_dict(vec)
            doc_times.append(per_doc)

    query_vectors: dict[str, dict[str, float]] = {}
    query_times: list[float] = []
    for q in queries:
        t0 = time.perf_counter()
        pooled = _sparse_encode_batch(model, tok, [q["query"]], device, max_length)
        elapsed = time.perf_counter() - t0
        query_vectors[q["query_id"]] = sparse_dict(pooled[0])
        query_times.append(elapsed)

    return {
        "model": model_name,
        "pooling": "max",
        "activation": "log1p_relu",
        "max_seq_length": max_length,
        "batch_size": batch_size,
        "n_docs": len(doc_vectors),
        "n_queries": len(query_vectors),
        "docs": doc_vectors,
        "queries": query_vectors,
        "timing": {
            "doc_encode_seconds": doc_times,
            "query_encode_seconds": query_times,
            "doc_mean_seconds": sum(doc_times) / len(doc_times),
            "query_mean_seconds": sum(query_times) / len(query_times),
            "doc_batch_size": batch_size,
        },
    }


def sparse_dot(a: dict[str, float], b: dict[str, float]) -> float:
    if len(a) > len(b):
        a, b = b, a
    return sum(w * b[t] for t, w in a.items() if t in b)


def splade_rank_for_query(
    query_vec: dict[str, float], doc_vectors: dict[str, dict[str, float]], pool: int
) -> list[str]:
    scored = [(key, sparse_dot(query_vec, dv)) for key, dv in doc_vectors.items()]
    scored.sort(key=lambda kv: (-kv[1], kv[0]))
    return [key for key, score in scored[:pool] if score > 0.0]


def rrf_fuse(rank_lists: list[list[str]], k: int, pool: int) -> list[str]:
    """`1 / (k + rank)` per leg, 1-based rank, summed across legs a key
    appears in (absent from a leg contributes 0 from that leg) — matches
    `rrf_score_one_based` (crates/khive-score/src/ops.rs:157) applied by
    `FusionStrategy::Rrf` (crates/khive-fusion/src/strategy.rs)."""
    scores: dict[str, float] = {}
    for ranked in rank_lists:
        for i, key in enumerate(ranked, start=1):
            scores[key] = scores.get(key, 0.0) + 1.0 / (k + i)
    ordered = sorted(scores.items(), key=lambda kv: (-kv[1], kv[0]))
    return [key for key, _ in ordered[:pool]]


def run_baseline_and_collect(
    kkernel: str, seed: int, epoch: str
) -> tuple[dict[str, str], dict[str, str], dict[str, list[str]], str]:
    """Reuses evaluate.py's own scratch/seed/run machinery (never
    reimplemented) to get, for the committed `A_fused_direct` condition: the
    key<->id maps and, per query_id, the ranked list of note *keys* in that
    condition's own top-`CANDIDATE_POOL` order."""
    import os

    kkernel_version = evaluate.get_kkernel_version(kkernel)
    evaluate.refuse_unsafe_db_env()
    root = evaluate.make_scratch(None)
    root_fd = evaluate.open_scratch_root_fd(root)
    try:
        db_path = root / "eval.db"
        evaluate._reject_existing_scratch_db(db_path)
        evaluate._claim_scratch_file(root_fd, "eval.db")
        known_children = evaluate.init_scratch_dirs(root_fd)
        try:
            with contextlib.ExitStack() as stack:
                home_vp = stack.enter_context(
                    evaluate.verified_scratch_path(root, root_fd, "home")
                )
                tmp_vp = stack.enter_context(
                    evaluate.verified_scratch_path(root, root_fd, "tmp")
                )
                env = evaluate.scratch_env(home_vp.path, tmp_vp.path, db_path)
                home_vp.recheck()
                tmp_vp.recheck()

            with evaluate.verified_scratch_path(root, root_fd, "eval.db") as migrate_vp:
                evaluate.run_kkernel(
                    kkernel, ["db", "migrate", "--db", migrate_vp.path], env
                )
                migrate_vp.recheck()

            queries = generate_corpus.parse_queries(DEFAULT_QUERIES)
            notes = evaluate.seed_corpus(kkernel, env, root, root_fd, seed, epoch)
            key_to_id = evaluate.key_id_map(root, root_fd)
            missing = [n["key"] for n in notes if n["key"] not in key_to_id]
            if missing:
                raise RuntimeError(
                    f"{len(missing)} seeded notes missing tag id, e.g. {missing[:5]}"
                )
            evaluate.set_ages(root, root_fd, notes, key_to_id)
            id_to_key = {v: k for k, v in key_to_id.items()}

            rows = evaluate.run_condition(
                kkernel, env, root, root_fd, "A_fused_direct", queries, len(notes)
            )
            baseline_ranked_keys: dict[str, list[str]] = {}
            for q, row in zip(queries, rows):
                hit_ids = [h["id"] for h in row["_hits"]]
                baseline_ranked_keys[q["query_id"]] = [
                    id_to_key[hid] for hid in hit_ids if hid in id_to_key
                ]
        finally:
            evaluate.cleanup_scratch(root, root_fd, known_children)
    finally:
        os.close(root_fd)
    return key_to_id, id_to_key, baseline_ranked_keys, kkernel_version


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--encode", action="store_true")
    ap.add_argument("--model", default="naver/splade-cocondenser-ensembledistil")
    ap.add_argument("--max-length", type=int, default=256)
    ap.add_argument("--batch-size", type=int, default=16)
    ap.add_argument(
        "--fixture", type=Path, default=HERE / "fixtures" / "splade_vectors_offline.json"
    )
    ap.add_argument("--kkernel", default="kkernel")
    ap.add_argument("--seed", type=int, default=generate_corpus.DEFAULT_SEED)
    ap.add_argument("--epoch", type=str, default=generate_corpus.DEFAULT_EPOCH)
    ap.add_argument("--rrf-k", type=int, default=RRF_K)
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--condition-name", default="B_splade_offline")
    args = ap.parse_args()

    if args.encode:
        fixture = encode_all(args.model, args.max_length, args.batch_size)
        args.fixture.parent.mkdir(parents=True, exist_ok=True)
        args.fixture.write_text(json.dumps(fixture, sort_keys=True) + "\n")
        print(f"wrote fixture: {args.fixture}")
        print(f"docs={fixture['n_docs']} queries={fixture['n_queries']}")
        print(f"doc encode mean: {fixture['timing']['doc_mean_seconds']*1000:.2f} ms/doc (batch={args.batch_size})")
        print(f"query encode mean: {fixture['timing']['query_mean_seconds']*1000:.2f} ms/query")
        return 0

    fixture = json.loads(args.fixture.read_text())
    doc_vectors = fixture["docs"]
    query_vectors = fixture["queries"]

    key_to_id, id_to_key, baseline_ranked_keys, kkernel_version = run_baseline_and_collect(
        args.kkernel, args.seed, args.epoch
    )
    print(f"kkernel version: {kkernel_version}")

    queries = generate_corpus.parse_queries(DEFAULT_QUERIES)
    per_query = []
    for q in queries:
        qid = q["query_id"]
        baseline_keys = baseline_ranked_keys[qid]
        splade_keys = splade_rank_for_query(
            query_vectors[qid], doc_vectors, SPLADE_POOL
        )
        fused_keys = rrf_fuse([baseline_keys, splade_keys], args.rrf_k, evaluate.CANDIDATE_POOL)
        fused_hits = [{"id": key_to_id[k]} for k in fused_keys if k in key_to_id]
        row = {"_hits": fused_hits}
        per_query.append(evaluate.compute_metrics(q, id_to_key, row))

    agg = evaluate.aggregate(per_query)
    evaluate.print_table(args.condition_name, agg)

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("w") as f:
            for r in per_query:
                f.write(json.dumps({"condition": args.condition_name, **r}) + "\n")
        print(f"\nwrote {len(per_query)} rows to {args.out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
