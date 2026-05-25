"""Param-tuning grid search for khive recall configuration.

Runs a FTS-only grid over scoring weights, candidate pool sizes, fusion
strategies, decay models, and temporal half-life parameters. One MCP session
is created and the corpus is loaded once; config is varied per recall() call.

TODO: Add --with-embed flag for embedding-enabled grid over both
      all-minilm-l6-v2 and paraphrase-multilingual-minilm-l12-v2 models.
      Requires no_embed=False and KHIVE_ADDITIONAL_EMBEDDING_MODELS=paraphrase.
"""

from __future__ import annotations

import argparse
import json
import time
from datetime import date
from pathlib import Path
from typing import Any

from khive_contract.client import KhiveMcpSession

RANDOM_SEED = 42

_HERE = Path(__file__).parent
DEFAULT_CORPUS = _HERE.parent / "fixtures" / "memories_corpus.json"
DEFAULT_OUTPUT = _HERE


# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------


def load_corpus(path: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Load memories and eval_queries from a corpus JSON file."""
    data = json.loads(path.read_text())
    memories: list[dict[str, Any]] = data["memories"]
    eval_queries: list[dict[str, Any]] = data["eval_queries"]
    return memories, eval_queries


# ---------------------------------------------------------------------------
# Session setup
# ---------------------------------------------------------------------------


def setup_session(
    memories: list[dict[str, Any]], db: str = ":memory:"
) -> tuple[KhiveMcpSession, dict[int, str]]:
    """Open a KhiveMcpSession and load all corpus memories via remember().

    The returned session is already entered (via __enter__). The caller MUST
    call session.close() when done, or use a try/finally block.

    Returns:
        (session, note_id_map) where note_id_map[corpus_index] = note_id string.
    """
    session = KhiveMcpSession(
        packs=("kg", "memory"),
        db=db,
        no_embed=True,
        log="error",
    )
    session.__enter__()

    note_id_map: dict[int, str] = {}
    total = len(memories)
    print(f"Loading {total} memories into session...")
    t_load_start = time.perf_counter()

    for i, mem in enumerate(memories):
        args: dict[str, Any] = {
            "content": mem["content"],
            "importance": mem["importance"],
            "decay_factor": mem["decay_factor"],
            "memory_type": mem["memory_type"],
        }
        if mem.get("tags"):
            args["tags"] = mem["tags"]

        result = session.verb("remember", args)
        note_id = result.get("note_id") or result.get("id") if result else None
        if not note_id:
            raise RuntimeError(f"remember() returned no note_id for memory {i}: {result!r}")
        note_id_map[i] = str(note_id)

        if (i + 1) % 25 == 0:
            elapsed = time.perf_counter() - t_load_start
            print(f"  Loaded {i + 1}/{total} memories ({elapsed:.1f}s)")

    elapsed = time.perf_counter() - t_load_start
    print(f"Corpus loaded in {elapsed:.1f}s. Beginning grid search...")
    return session, note_id_map


# ---------------------------------------------------------------------------
# Metric evaluation
# ---------------------------------------------------------------------------


def evaluate_config(
    session: KhiveMcpSession,
    config_dict: dict[str, Any],
    eval_queries: list[dict[str, Any]],
    note_id_map: dict[int, str],
) -> dict[str, float]:
    """Evaluate one RecallConfig against all eval queries.

    Returns:
        {"recall_at_10": float, "mrr": float, "mean_latency_ms": float}
    """
    recalls: list[float] = []
    mrrs: list[float] = []
    latencies: list[float] = []

    for eq in eval_queries:
        query: str = eq["query"]
        relevant_indices: list[int] = eq["relevant_indices"]
        relevant_note_ids = {note_id_map[i] for i in relevant_indices if i in note_id_map}

        t0 = time.perf_counter()
        try:
            hits = session.verb(
                "recall",
                {"query": query, "limit": 10, "config": config_dict},
            )
        except Exception:
            hits = []
        latency_ms = (time.perf_counter() - t0) * 1000.0
        latencies.append(latency_ms)

        retrieved_ids: list[str] = []
        if isinstance(hits, list):
            for h in hits:
                nid = h.get("note_id") or h.get("id") if isinstance(h, dict) else None
                if nid:
                    retrieved_ids.append(str(nid))

        # recall@10
        retrieved_set = set(retrieved_ids)
        if relevant_note_ids:
            r_at_10 = len(relevant_note_ids & retrieved_set) / len(relevant_note_ids)
        else:
            r_at_10 = 0.0
        recalls.append(r_at_10)

        # MRR — reciprocal rank of first relevant hit
        mrr = 0.0
        for rank, nid in enumerate(retrieved_ids, 1):
            if nid in relevant_note_ids:
                mrr = 1.0 / rank
                break
        mrrs.append(mrr)

    n = len(eval_queries)
    return {
        "recall_at_10": sum(recalls) / n if n else 0.0,
        "mrr": sum(mrrs) / n if n else 0.0,
        "mean_latency_ms": sum(latencies) / n if n else 0.0,
    }


# ---------------------------------------------------------------------------
# Grid generation
# ---------------------------------------------------------------------------


def generate_grid(quick: bool = False) -> list[dict[str, Any]]:
    """Generate the FTS-only RecallConfig parameter grid.

    Full grid:  4 × 4 × 8 × 3 × 3 = 1152 configs
    Quick grid: every 10th config (deterministic sort) ≈ 116 configs

    Weight triples are normalized so relevance+importance+temporal = 1.0.
    Weighted fusion uses [text_weight, vector_weight] where alpha=vector_weight.
    In FTS-only mode (no_embed=True) all vector results are empty, so
    weighted configs with high vector alpha will score poorly — this is
    expected and meaningful for the grid.
    """
    weight_triples = [
        # (relevance_weight, importance_weight, temporal_weight)
        (0.70, 0.20, 0.10),  # default
        (0.60, 0.30, 0.10),
        (0.60, 0.20, 0.20),
        (0.80, 0.10, 0.10),
    ]

    candidate_pools = [
        # (candidate_multiplier, candidate_limit)
        (10, None),
        (20, None),   # default
        (40, None),
        (20, 100),
    ]

    # 3 RRF + 5 weighted = 8 fusion configs
    fusion_configs: list[dict[str, Any]] = [
        {"rrf": {"k": 20}},
        {"rrf": {"k": 60}},   # default
        {"rrf": {"k": 100}},
        {"weighted": {"weights": [1.0, 0.0]}},    # text-only
        {"weighted": {"weights": [0.75, 0.25]}},
        {"weighted": {"weights": [0.5, 0.5]}},
        {"weighted": {"weights": [0.25, 0.75]}},
        {"weighted": {"weights": [0.0, 1.0]}},    # vector-only
    ]

    decay_models = ["exponential", "hyperbolic", "none"]
    half_lives = [14.0, 30.0, 60.0]

    configs: list[dict[str, Any]] = []
    for rw, iw, tw in weight_triples:
        for cm, cl in candidate_pools:
            for fuse in fusion_configs:
                for decay in decay_models:
                    for hl in half_lives:
                        cfg: dict[str, Any] = {
                            "relevance_weight": rw,
                            "importance_weight": iw,
                            "temporal_weight": tw,
                            "candidate_multiplier": cm,
                            "fuse_strategy": fuse,
                            "decay_model": decay,
                            "temporal_half_life_days": hl,
                            "min_score": 0.0,
                            "min_salience": 0.0,
                        }
                        if cl is not None:
                            cfg["candidate_limit"] = cl
                        configs.append(cfg)

    if quick:
        configs = configs[::10]

    return configs


# ---------------------------------------------------------------------------
# Grid execution
# ---------------------------------------------------------------------------


def run_grid(
    session: KhiveMcpSession,
    grid: list[dict[str, Any]],
    eval_queries: list[dict[str, Any]],
    note_id_map: dict[int, str],
) -> list[dict[str, Any]]:
    """Run evaluate_config for every config in the grid.

    MCP is single-threaded stdio, so iteration is sequential.
    Prints progress every 100 configs.

    Returns:
        List of result dicts: {"config_index", "config", "recall_at_10", "mrr", "mean_latency_ms"}
    """
    results: list[dict[str, Any]] = []
    total = len(grid)

    for i, config in enumerate(grid):
        if i % 100 == 0:
            print(f"  [{i}/{total}] config {i}...")
        metrics = evaluate_config(session, config, eval_queries, note_id_map)
        results.append(
            {
                "config_index": i,
                "config": config,
                **metrics,
            }
        )

    return results


# ---------------------------------------------------------------------------
# Result writing
# ---------------------------------------------------------------------------


def _fuse_to_toml(fuse: dict[str, Any] | str) -> str:
    """Render a fuse_strategy value as a TOML inline table or string."""
    if isinstance(fuse, str):
        return f'"{fuse}"'
    if "rrf" in fuse:
        k = fuse["rrf"]["k"]
        return f"{{rrf = {{k = {k}}}}}"
    if "weighted" in fuse:
        weights = fuse["weighted"]["weights"]
        return f"{{weighted = {{weights = [{weights[0]}, {weights[1]}]}}}}"
    # fallback: JSON-encode as a TOML comment note
    return f'"{json.dumps(fuse)}"'


def write_results(
    results: list[dict[str, Any]],
    output_dir: Path,
    *,
    t_total_seconds: float,
    default_config_metrics: dict[str, float] | None = None,
) -> None:
    """Write results.json, tuned-config.toml, and REPORT.md to output_dir."""
    output_dir.mkdir(parents=True, exist_ok=True)
    t_total = t_total_seconds
    today = date.today().isoformat()

    # --- results.json ---
    (output_dir / "results.json").write_text(json.dumps(results, indent=2))
    print(f"Wrote {output_dir / 'results.json'} ({len(results)} configs)")

    # --- rank by recall@10 then MRR ---
    sorted_by_recall = sorted(
        results, key=lambda r: (r["recall_at_10"], r["mrr"]), reverse=True
    )
    sorted_by_mrr = sorted(
        results, key=lambda r: (r["mrr"], r["recall_at_10"]), reverse=True
    )
    winner = sorted_by_recall[0]
    cfg = winner["config"]

    # --- tuned-config.toml ---
    fuse_toml = _fuse_to_toml(cfg["fuse_strategy"])
    decay_model_str = cfg["decay_model"] if isinstance(cfg["decay_model"], str) else json.dumps(cfg["decay_model"])
    cl_line = (
        f"candidate_limit = {cfg['candidate_limit']}"
        if cfg.get("candidate_limit") is not None
        else "# candidate_limit = null  (use multiplier only)"
    )
    toml_content = f"""\
# Winning config from khive recall param-tuning grid search
# run_date = "{today}"
# recall_at_10 = {winner['recall_at_10']:.4f}
# mrr = {winner['mrr']:.4f}
# mean_latency_ms = {winner['mean_latency_ms']:.2f}

[recall]
relevance_weight = {cfg['relevance_weight']}
importance_weight = {cfg['importance_weight']}
temporal_weight = {cfg['temporal_weight']}
temporal_half_life_days = {cfg['temporal_half_life_days']}
decay_model = "{decay_model_str}"
candidate_multiplier = {cfg['candidate_multiplier']}
{cl_line}
fuse_strategy = {fuse_toml}
min_score = {cfg['min_score']}
min_salience = {cfg['min_salience']}
"""
    (output_dir / "tuned-config.toml").write_text(toml_content)
    print(f"Wrote {output_dir / 'tuned-config.toml'}")

    # --- REPORT.md ---
    top10_recall = sorted_by_recall[:10]
    top10_mrr = sorted_by_mrr[:10]

    def _cfg_summary(r: dict[str, Any]) -> str:
        c = r["config"]
        fuse = c["fuse_strategy"]
        if isinstance(fuse, dict) and "rrf" in fuse:
            fuse_str = f"rrf(k={fuse['rrf']['k']})"
        elif isinstance(fuse, dict) and "weighted" in fuse:
            w = fuse["weighted"]["weights"]
            fuse_str = f"weighted({w[0]}/{w[1]})"
        else:
            fuse_str = str(fuse)
        decay_str = c["decay_model"] if isinstance(c["decay_model"], str) else json.dumps(c["decay_model"])
        return (
            f"rel={c['relevance_weight']} imp={c['importance_weight']} "
            f"tmp={c['temporal_weight']} cand={c['candidate_multiplier']} "
            f"fuse={fuse_str} decay={decay_str} hl={c['temporal_half_life_days']}"
        )

    def _row(r: dict[str, Any]) -> str:
        return (
            f"| {r['config_index']:4d} | {r['recall_at_10']:.4f} | {r['mrr']:.4f} "
            f"| {r['mean_latency_ms']:.1f}ms | {_cfg_summary(r)} |"
        )

    top10_recall_rows = "\n".join(_row(r) for r in top10_recall)
    top10_mrr_rows = "\n".join(_row(r) for r in top10_mrr)

    default_section = ""
    if default_config_metrics:
        default_section = f"""
## Default vs Tuned Comparison

| Metric | Default config | Tuned config | Delta |
|--------|---------------|-------------|-------|
| recall@10 | {default_config_metrics['recall_at_10']:.4f} | {winner['recall_at_10']:.4f} | {winner['recall_at_10'] - default_config_metrics['recall_at_10']:+.4f} |
| MRR | {default_config_metrics['mrr']:.4f} | {winner['mrr']:.4f} | {winner['mrr'] - default_config_metrics['mrr']:+.4f} |
| mean latency | {default_config_metrics['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms'] - default_config_metrics['mean_latency_ms']:+.1f}ms |

Default config: relevance=0.70 importance=0.20 temporal=0.10 candidate_multiplier=20 fuse=rrf(k=60) decay=exponential half_life=30.0
"""

    report = f"""\
# Param-Tuning Grid Search Report

- **Date**: {today}
- **Grid size**: {len(results)} configs
- **Eval queries**: 20
- **Total runtime**: {t_total:.1f}s
- **Mode**: FTS-only (no_embed=True)

## Winning Config (highest recall@10)

| Metric | Value |
|--------|-------|
| recall@10 | {winner['recall_at_10']:.4f} |
| MRR | {winner['mrr']:.4f} |
| mean latency | {winner['mean_latency_ms']:.1f}ms |
| config_index | {winner['config_index']} |

Parameters: `{_cfg_summary(winner)}`
{default_section}
## Top 10 by recall@10

| idx | recall@10 | mrr | latency | config |
|-----|-----------|-----|---------|--------|
{top10_recall_rows}

## Top 10 by MRR

| idx | recall@10 | mrr | latency | config |
|-----|-----------|-----|---------|--------|
{top10_mrr_rows}
"""
    (output_dir / "REPORT.md").write_text(report)
    print(f"Wrote {output_dir / 'REPORT.md'}")


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

_DEFAULT_CONFIG = {
    "relevance_weight": 0.70,
    "importance_weight": 0.20,
    "temporal_weight": 0.10,
    "candidate_multiplier": 20,
    "fuse_strategy": {"rrf": {"k": 60}},
    "decay_model": "exponential",
    "temporal_half_life_days": 30.0,
    "min_score": 0.0,
    "min_salience": 0.0,
}


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Grid search for khive recall config parameters (FTS-only mode)."
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="Sample every 10th config for a fast smoke test (~10x faster).",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="Directory to write results.json, tuned-config.toml, REPORT.md.",
    )
    parser.add_argument(
        "--corpus",
        type=Path,
        default=DEFAULT_CORPUS,
        help="Path to memories_corpus.json fixture.",
    )
    args = parser.parse_args()

    corpus_path: Path = args.corpus
    output_dir: Path = args.output_dir

    if not corpus_path.exists():
        raise FileNotFoundError(f"Corpus not found: {corpus_path}")

    print(f"Loading corpus from {corpus_path}")
    memories, eval_queries = load_corpus(corpus_path)
    print(f"Corpus: {len(memories)} memories, {len(eval_queries)} eval queries")

    grid = generate_grid(quick=args.quick)
    print(f"Grid: {len(grid)} configs (quick={args.quick})")

    t_start = time.perf_counter()
    session, note_id_map = setup_session(memories)
    try:
        # Evaluate default config for the comparison table
        default_metrics = evaluate_config(session, _DEFAULT_CONFIG, eval_queries, note_id_map)
        print(
            f"Default config: recall@10={default_metrics['recall_at_10']:.4f} "
            f"mrr={default_metrics['mrr']:.4f}"
        )

        results = run_grid(session, grid, eval_queries, note_id_map)
    finally:
        session.close()

    t_elapsed = time.perf_counter() - t_start
    print(f"Grid search complete in {t_elapsed:.1f}s")

    write_results(
        results,
        output_dir,
        t_total_seconds=t_elapsed,
        default_config_metrics=default_metrics,
    )

    best = max(results, key=lambda r: (r["recall_at_10"], r["mrr"]))
    print(
        f"\nBest config: recall@10={best['recall_at_10']:.4f} mrr={best['mrr']:.4f} "
        f"(index {best['config_index']})"
    )
    print(f"Results written to {output_dir}")


if __name__ == "__main__":
    main()
