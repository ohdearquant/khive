"""Param-tuning grid search for khive recall configuration.

Runs a FTS-only grid over scoring weights, candidate pool sizes, fusion
strategies, decay models, and temporal half-life parameters. One MCP session
is created and the corpus is loaded once; config is varied per recall() call.

Supports two corpus schemas:
  v1 (memories_corpus.json):   eval_queries use ``relevant_indices`` (int list)
  v2 (memories_corpus_v2.json): eval_queries use ``expected_top_k`` (corpus ID
      strings like "mem_001") and optional ``expected_excluded``.

Discriminating metrics (v2 corpus):
  MRR_expected: mean reciprocal rank of the FIRST expected_top_k hit in results.
      Sensitive to ranking order — a hit at rank 1 scores 1.0, rank 5 scores 0.2.
  precision_at_k: fraction of expected_top_k that appear in the top-k results,
      where k = len(expected_top_k). Sensitive to candidate pool and score weights.
  exclusion_penalty: fraction of expected_excluded items that appear in top-10,
      penalising configs that surface distractors. Subtracted from final score.

Combined discriminating score = 0.5 * MRR_expected + 0.3 * precision_at_k
                                 - 0.2 * exclusion_penalty

The v1 recall_at_10 metric is retained for backwards compatibility when a v1
corpus is loaded.

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

# Weight constants for the combined discriminating score (v2 only)
_MRR_WEIGHT = 0.5
_PREC_WEIGHT = 0.3
_EXCL_WEIGHT = 0.2


# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------


def _detect_corpus_version(data: dict[str, Any]) -> str:
    """Return "v2" if any eval query uses expected_top_k, else "v1"."""
    for eq in data.get("eval_queries", []):
        if "expected_top_k" in eq:
            return "v2"
    return "v1"


def load_corpus(path: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]], str]:
    """Load memories and eval_queries from a corpus JSON file.

    Returns:
        (memories, eval_queries, version) where version is "v1" or "v2".
    """
    data = json.loads(path.read_text())
    memories: list[dict[str, Any]] = data["memories"]
    eval_queries: list[dict[str, Any]] = data["eval_queries"]
    version = _detect_corpus_version(data)
    return memories, eval_queries, version


# ---------------------------------------------------------------------------
# Session setup
# ---------------------------------------------------------------------------


def setup_session(
    memories: list[dict[str, Any]], db: str = ":memory:", *, version: str = "v1"
) -> tuple[KhiveMcpSession, dict[str, str]]:
    """Open a KhiveMcpSession and load all corpus memories via remember().

    The returned session is already entered (via __enter__). The caller MUST
    call session.close() when done, or use a try/finally block.

    For v1 corpus the key is the integer index (as a string).
    For v2 corpus the key is the corpus ID string (e.g. "mem_001").

    Returns:
        (session, note_id_map) where note_id_map[corpus_key] = khive note_id.
    """
    session = KhiveMcpSession(
        packs=("kg", "memory"),
        db=db,
        no_embed=True,
        log="error",
    )
    session.__enter__()

    note_id_map: dict[str, str] = {}
    total = len(memories)
    print(f"Loading {total} memories into session...")
    t_load_start = time.perf_counter()

    for i, mem in enumerate(memories):
        args: dict[str, Any] = {
            "content": mem["content"],
            "salience": mem["salience"],
            "decay_factor": mem["decay_factor"],
            "memory_type": mem["memory_type"],
        }
        if mem.get("tags"):
            args["tags"] = mem["tags"]

        result = session.verb("remember", args)
        note_id = result["id"] if result else None
        if not note_id:
            raise RuntimeError(f"remember() returned no id for memory {i}: {result!r}")

        if version == "v2":
            corpus_key = mem.get("id", str(i))
        else:
            corpus_key = str(i)
        note_id_map[corpus_key] = str(note_id)

        if (i + 1) % 25 == 0:
            elapsed = time.perf_counter() - t_load_start
            print(f"  Loaded {i + 1}/{total} memories ({elapsed:.1f}s)")

    elapsed = time.perf_counter() - t_load_start
    print(f"Corpus loaded in {elapsed:.1f}s. Beginning grid search...")
    return session, note_id_map


# ---------------------------------------------------------------------------
# Metric evaluation
# ---------------------------------------------------------------------------


def _resolve_relevant_ids_v1(
    eq: dict[str, Any], note_id_map: dict[str, str]
) -> set[str]:
    """Resolve v1 relevant_indices to khive note IDs."""
    return {note_id_map[str(i)] for i in eq.get("relevant_indices", []) if str(i) in note_id_map}


def _resolve_relevant_ids_v2(
    eq: dict[str, Any], note_id_map: dict[str, str]
) -> tuple[list[str], set[str]]:
    """Resolve v2 expected_top_k and expected_excluded to khive note IDs.

    Returns:
        (expected_top_k_ids, excluded_ids) where each element is a khive note ID.
    """
    top_k_ids = [note_id_map[cid] for cid in eq.get("expected_top_k", []) if cid in note_id_map]
    excluded_ids = {note_id_map[cid] for cid in eq.get("expected_excluded", []) if cid in note_id_map}
    return top_k_ids, excluded_ids


def evaluate_config(
    session: KhiveMcpSession,
    config_dict: dict[str, Any],
    eval_queries: list[dict[str, Any]],
    note_id_map: dict[str, str],
    *,
    version: str = "v1",
) -> dict[str, float]:
    """Evaluate one RecallConfig against all eval queries.

    v1 returns: {"recall_at_10", "mrr", "mean_latency_ms"}
    v2 returns: {"recall_at_10", "mrr", "mrr_expected", "precision_at_k",
                 "exclusion_penalty", "combined_score", "mean_latency_ms"}

    mrr_expected (v2): mean reciprocal rank of the FIRST expected_top_k hit.
        Unlike the v1 MRR which considers ANY relevant memory, this measures
        whether the *specifically expected* items appear early in results.
    precision_at_k (v2): fraction of expected_top_k found in the top-k results
        where k = len(expected_top_k). Penalises configs that miss specific targets.
    exclusion_penalty (v2): fraction of expected_excluded that appear in top-10.
        Non-zero means distractors are being surfaced above relevant items.
    combined_score (v2): 0.5*mrr_expected + 0.3*precision_at_k - 0.2*exclusion_penalty
        This is the primary discriminating metric for v2 grid ranking.
    """
    recalls: list[float] = []
    mrrs: list[float] = []
    latencies: list[float] = []

    # v2-only accumulators
    mrrs_expected: list[float] = []
    precisions_at_k: list[float] = []
    exclusion_penalties: list[float] = []

    for eq in eval_queries:
        query: str = eq["query"]

        if version == "v2":
            expected_top_k_ids, excluded_ids = _resolve_relevant_ids_v2(eq, note_id_map)
            relevant_note_ids = set(expected_top_k_ids)
            limit = max(10, len(expected_top_k_ids))
        else:
            relevant_note_ids = _resolve_relevant_ids_v1(eq, note_id_map)
            excluded_ids = set()
            limit = 10

        t0 = time.perf_counter()
        try:
            hits = session.verb(
                "recall",
                {"query": query, "limit": limit, "config": config_dict},
            )
        except Exception:
            hits = []
        latency_ms = (time.perf_counter() - t0) * 1000.0
        latencies.append(latency_ms)

        retrieved_ids: list[str] = []
        if isinstance(hits, list):
            for h in hits:
                nid = h.get("id") if isinstance(h, dict) else None
                if nid:
                    retrieved_ids.append(str(nid))

        retrieved_set = set(retrieved_ids)
        top10_set = set(retrieved_ids[:10])

        # --- recall@10 (both versions) ---
        if relevant_note_ids:
            r_at_10 = len(relevant_note_ids & top10_set) / len(relevant_note_ids)
        else:
            r_at_10 = 0.0
        recalls.append(r_at_10)

        # --- MRR v1: reciprocal rank of first ANY relevant hit ---
        mrr_v1 = 0.0
        for rank, nid in enumerate(retrieved_ids, 1):
            if nid in relevant_note_ids:
                mrr_v1 = 1.0 / rank
                break
        mrrs.append(mrr_v1)

        if version == "v2":
            # --- MRR_expected: reciprocal rank of first expected_top_k hit ---
            mrr_exp = 0.0
            for rank, nid in enumerate(retrieved_ids, 1):
                if nid in set(expected_top_k_ids):
                    mrr_exp = 1.0 / rank
                    break
            mrrs_expected.append(mrr_exp)

            # --- precision@k: fraction of expected_top_k in top-k results ---
            k = len(expected_top_k_ids)
            if k > 0:
                top_k_retrieved = set(retrieved_ids[:k])
                prec_at_k = len(set(expected_top_k_ids) & top_k_retrieved) / k
            else:
                prec_at_k = 0.0
            precisions_at_k.append(prec_at_k)

            # --- exclusion_penalty: distractors surfaced in top-10 ---
            if excluded_ids:
                penalty = len(excluded_ids & top10_set) / len(excluded_ids)
            else:
                penalty = 0.0
            exclusion_penalties.append(penalty)

    n = len(eval_queries)
    base: dict[str, float] = {
        "recall_at_10": sum(recalls) / n if n else 0.0,
        "mrr": sum(mrrs) / n if n else 0.0,
        "mean_latency_ms": sum(latencies) / n if n else 0.0,
    }

    if version == "v2":
        mrr_exp_mean = sum(mrrs_expected) / n if n else 0.0
        prec_mean = sum(precisions_at_k) / n if n else 0.0
        excl_mean = sum(exclusion_penalties) / n if n else 0.0
        combined = _MRR_WEIGHT * mrr_exp_mean + _PREC_WEIGHT * prec_mean - _EXCL_WEIGHT * excl_mean
        base.update({
            "mrr_expected": mrr_exp_mean,
            "precision_at_k": prec_mean,
            "exclusion_penalty": excl_mean,
            "combined_score": combined,
        })

    return base


# ---------------------------------------------------------------------------
# Grid generation
# ---------------------------------------------------------------------------


def generate_grid(quick: bool = False) -> list[dict[str, Any]]:
    """Generate the FTS-only RecallConfig parameter grid.

    Full grid (v2): 4 × 4 × 8 × 3 × 3 × 2 = 2304 configs
    Quick grid:     every 20th config (deterministic sort) ≈ 116 configs

    v2 adds min_salience variation which DOES discriminate on the v2 corpus:
    - min_salience=0.0: retrieves all items including low-salience traps
    - min_salience=0.4: filters out low-salience items (0.28-0.35) from results

    Weight triples are normalized so relevance+salience+temporal = 1.0.
    Weighted fusion uses [text_weight, vector_weight] where alpha=vector_weight.
    In FTS-only mode (no_embed=True) all vector results are empty, so
    weighted configs with high vector alpha will score poorly — this is
    expected and meaningful for the grid.
    """
    weight_triples = [
        # (relevance_weight, salience_weight, temporal_weight)
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

    # min_salience variation: the key discriminating dimension for v2 corpus.
    # 0.0 includes all items (even low-salience traps).
    # 0.40 filters out importance_trap memories (salience 0.26-0.35) from results,
    # which FAILS importance_trap queries (those expected items have salience < 0.40).
    # This creates measurable discrimination: configs with min_salience=0.0 find trap
    # items; configs with min_salience=0.40 do not.
    min_salience_values = [0.0, 0.40]

    configs: list[dict[str, Any]] = []
    for rw, iw, tw in weight_triples:
        for cm, cl in candidate_pools:
            for fuse in fusion_configs:
                for decay in decay_models:
                    for hl in half_lives:
                        for ms in min_salience_values:
                            cfg: dict[str, Any] = {
                                "relevance_weight": rw,
                                "salience_weight": iw,
                                "temporal_weight": tw,
                                "candidate_multiplier": cm,
                                "fuse_strategy": fuse,
                                "decay_model": decay,
                                "temporal_half_life_days": hl,
                                "min_score": 0.0,
                                "min_salience": ms,
                            }
                            if cl is not None:
                                cfg["candidate_limit"] = cl
                            configs.append(cfg)

    if quick:
        # Sample the grid to get ~116 configs while preserving both min_salience values.
        # Because min_salience alternates as the innermost dimension, we take every 10th
        # config from the even positions (ms=0.0 group) and every 10th from the odd
        # positions (ms=0.4 group), then interleave them. This ensures both values appear.
        ms0_configs = configs[::2]   # every even index = min_salience=0.0
        ms04_configs = configs[1::2] # every odd index = min_salience=0.40
        sampled_ms0 = ms0_configs[::10]    # ~58 configs
        sampled_ms04 = ms04_configs[::10]  # ~58 configs
        configs = sampled_ms0 + sampled_ms04  # ~116 total

    return configs


# ---------------------------------------------------------------------------
# Grid execution
# ---------------------------------------------------------------------------


def run_grid(
    session: KhiveMcpSession,
    grid: list[dict[str, Any]],
    eval_queries: list[dict[str, Any]],
    note_id_map: dict[str, str],
    *,
    version: str = "v1",
) -> list[dict[str, Any]]:
    """Run evaluate_config for every config in the grid.

    MCP is single-threaded stdio, so iteration is sequential.
    Prints progress every 100 configs.

    Returns:
        List of result dicts with metrics appropriate to the corpus version.
    """
    results: list[dict[str, Any]] = []
    total = len(grid)

    for i, config in enumerate(grid):
        if i % 100 == 0:
            print(f"  [{i}/{total}] config {i}...")
        metrics = evaluate_config(session, config, eval_queries, note_id_map, version=version)
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
    n_eval_queries: int,
    default_config_metrics: dict[str, float] | None = None,
    version: str = "v1",
    report_filename: str = "REPORT.md",
) -> None:
    """Write results.json, tuned-config.toml, and REPORT.md (or REPORT-v2.md) to output_dir."""
    output_dir.mkdir(parents=True, exist_ok=True)
    t_total = t_total_seconds
    today = date.today().isoformat()

    # --- results.json ---
    results_filename = "results.json" if version == "v1" else "results_v2.json"
    (output_dir / results_filename).write_text(json.dumps(results, indent=2))
    print(f"Wrote {output_dir / results_filename} ({len(results)} configs)")

    # --- choose primary sort key ---
    if version == "v2":
        primary_sort_key = "combined_score"
    else:
        primary_sort_key = "recall_at_10"

    sorted_primary = sorted(
        results, key=lambda r: (r[primary_sort_key], r.get("mrr", 0.0)), reverse=True
    )
    sorted_by_mrr = sorted(
        results, key=lambda r: (r.get("mrr_expected", r.get("mrr", 0.0)), r[primary_sort_key]),
        reverse=True,
    )
    winner = sorted_primary[0]
    cfg = winner["config"]

    # --- tuned-config.toml ---
    toml_filename = "tuned-config.toml" if version == "v1" else "tuned-config-v2.toml"
    fuse_toml = _fuse_to_toml(cfg["fuse_strategy"])
    decay_model_str = cfg["decay_model"] if isinstance(cfg["decay_model"], str) else json.dumps(cfg["decay_model"])
    cl_line = (
        f"candidate_limit = {cfg['candidate_limit']}"
        if cfg.get("candidate_limit") is not None
        else "# candidate_limit = null  (use multiplier only)"
    )
    score_comment = (
        f"# combined_score = {winner['combined_score']:.4f}  "
        f"(mrr_expected={winner['mrr_expected']:.4f} precision_at_k={winner['precision_at_k']:.4f} "
        f"exclusion_penalty={winner['exclusion_penalty']:.4f})"
        if version == "v2"
        else f"# recall_at_10 = {winner['recall_at_10']:.4f}"
    )
    toml_content = f"""\
# Winning config from khive recall param-tuning grid search
# run_date = "{today}"
{score_comment}
# mrr = {winner.get('mrr', 0.0):.4f}
# mean_latency_ms = {winner['mean_latency_ms']:.2f}

[recall]
relevance_weight = {cfg['relevance_weight']}
salience_weight = {cfg['salience_weight']}
temporal_weight = {cfg['temporal_weight']}
temporal_half_life_days = {cfg['temporal_half_life_days']}
decay_model = "{decay_model_str}"
candidate_multiplier = {cfg['candidate_multiplier']}
{cl_line}
fuse_strategy = {fuse_toml}
min_score = {cfg['min_score']}
min_salience = {cfg['min_salience']}
"""
    (output_dir / toml_filename).write_text(toml_content)
    print(f"Wrote {output_dir / toml_filename}")

    # --- REPORT.md ---
    top10_primary = sorted_primary[:10]
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
        ms = c.get("min_salience", 0.0)
        return (
            f"rel={c['relevance_weight']} sal={c['salience_weight']} "
            f"tmp={c['temporal_weight']} cand={c['candidate_multiplier']} "
            f"fuse={fuse_str} decay={decay_str} hl={c['temporal_half_life_days']} "
            f"ms={ms}"
        )

    if version == "v2":
        def _row(r: dict[str, Any]) -> str:
            return (
                f"| {r['config_index']:4d} "
                f"| {r['combined_score']:.4f} "
                f"| {r['mrr_expected']:.4f} "
                f"| {r['precision_at_k']:.4f} "
                f"| {r['exclusion_penalty']:.4f} "
                f"| {r['mean_latency_ms']:.1f}ms "
                f"| {_cfg_summary(r)} |"
            )
        header = "| idx | combined | mrr_exp | prec@k | excl_pen | latency | config |"
        divider = "|-----|---------|---------|--------|----------|---------|--------|"
    else:
        def _row(r: dict[str, Any]) -> str:
            return (
                f"| {r['config_index']:4d} | {r['recall_at_10']:.4f} | {r['mrr']:.4f} "
                f"| {r['mean_latency_ms']:.1f}ms | {_cfg_summary(r)} |"
            )
        header = "| idx | recall@10 | mrr | latency | config |"
        divider = "|-----|-----------|-----|---------|--------|"

    top10_primary_rows = "\n".join(_row(r) for r in top10_primary)
    top10_mrr_rows = "\n".join(_row(r) for r in top10_mrr)

    default_section = ""
    if default_config_metrics:
        if version == "v2":
            default_section = f"""
## Default vs Tuned Comparison

| Metric | Default config | Tuned config | Delta |
|--------|---------------|-------------|-------|
| combined_score | {default_config_metrics['combined_score']:.4f} | {winner['combined_score']:.4f} | {winner['combined_score'] - default_config_metrics['combined_score']:+.4f} |
| mrr_expected | {default_config_metrics['mrr_expected']:.4f} | {winner['mrr_expected']:.4f} | {winner['mrr_expected'] - default_config_metrics['mrr_expected']:+.4f} |
| precision_at_k | {default_config_metrics['precision_at_k']:.4f} | {winner['precision_at_k']:.4f} | {winner['precision_at_k'] - default_config_metrics['precision_at_k']:+.4f} |
| exclusion_penalty | {default_config_metrics['exclusion_penalty']:.4f} | {winner['exclusion_penalty']:.4f} | {winner['exclusion_penalty'] - default_config_metrics['exclusion_penalty']:+.4f} |
| recall_at_10 | {default_config_metrics['recall_at_10']:.4f} | {winner['recall_at_10']:.4f} | {winner['recall_at_10'] - default_config_metrics['recall_at_10']:+.4f} |
| mean latency | {default_config_metrics['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms'] - default_config_metrics['mean_latency_ms']:+.1f}ms |

Default config: relevance=0.70 salience=0.20 temporal=0.10 candidate_multiplier=20 fuse=rrf(k=60) decay=exponential half_life=30.0
"""
        else:
            default_section = f"""
## Default vs Tuned Comparison

| Metric | Default config | Tuned config | Delta |
|--------|---------------|-------------|-------|
| recall@10 | {default_config_metrics['recall_at_10']:.4f} | {winner['recall_at_10']:.4f} | {winner['recall_at_10'] - default_config_metrics['recall_at_10']:+.4f} |
| MRR | {default_config_metrics['mrr']:.4f} | {winner['mrr']:.4f} | {winner['mrr'] - default_config_metrics['mrr']:+.4f} |
| mean latency | {default_config_metrics['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms']:.1f}ms | {winner['mean_latency_ms'] - default_config_metrics['mean_latency_ms']:+.1f}ms |

Default config: relevance=0.70 salience=0.20 temporal=0.10 candidate_multiplier=20 fuse=rrf(k=60) decay=exponential half_life=30.0
"""

    if version == "v2":
        winning_metrics_table = f"""\
| combined_score | {winner['combined_score']:.4f} |
| mrr_expected | {winner['mrr_expected']:.4f} |
| precision_at_k | {winner['precision_at_k']:.4f} |
| exclusion_penalty | {winner['exclusion_penalty']:.4f} |
| recall_at_10 | {winner['recall_at_10']:.4f} |
| mrr (v1) | {winner['mrr']:.4f} |
| mean latency | {winner['mean_latency_ms']:.1f}ms |
| config_index | {winner['config_index']} |"""
        # compute score ranges to document discrimination
        combined_scores = sorted(set(round(r["combined_score"], 4) for r in results))
        mrr_exp_scores = sorted(set(round(r["mrr_expected"], 4) for r in results))
        prec_scores = sorted(set(round(r["precision_at_k"], 4) for r in results))
        discrimination_section = f"""
## Discrimination Analysis (v2 corpus)

| Metric | Distinct values | Min | Max | Range |
|--------|-----------------|-----|-----|-------|
| combined_score | {len(combined_scores)} | {min(combined_scores):.4f} | {max(combined_scores):.4f} | {max(combined_scores) - min(combined_scores):.4f} |
| mrr_expected | {len(mrr_exp_scores)} | {min(mrr_exp_scores):.4f} | {max(mrr_exp_scores):.4f} | {max(mrr_exp_scores) - min(mrr_exp_scores):.4f} |
| precision_at_k | {len(prec_scores)} | {min(prec_scores):.4f} | {max(prec_scores):.4f} | {max(prec_scores) - min(prec_scores):.4f} |

A non-flat landscape requires combined_score range > 0.05 across configs.
"""
    else:
        winning_metrics_table = f"""\
| recall@10 | {winner['recall_at_10']:.4f} |
| MRR | {winner['mrr']:.4f} |
| mean latency | {winner['mean_latency_ms']:.1f}ms |
| config_index | {winner['config_index']} |"""
        discrimination_section = ""

    report = f"""\
# Param-Tuning Grid Search Report

- **Date**: {today}
- **Corpus version**: {version}
- **Grid size**: {len(results)} configs
- **Eval queries**: {n_eval_queries}
- **Total runtime**: {t_total:.1f}s
- **Mode**: FTS-only (no_embed=True)

## Winning Config (highest {primary_sort_key})

| Metric | Value |
|--------|-------|
{winning_metrics_table}

Parameters: `{_cfg_summary(winner)}`
{default_section}{discrimination_section}
## Top 10 by {primary_sort_key}

{header}
{divider}
{top10_primary_rows}

## Top 10 by MRR

{header}
{divider}
{top10_mrr_rows}
"""
    (output_dir / report_filename).write_text(report)
    print(f"Wrote {output_dir / report_filename}")


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

_DEFAULT_CONFIG = {
    "relevance_weight": 0.70,
    "salience_weight": 0.20,
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
        help="Path to memories corpus JSON fixture (v1 or v2 schema auto-detected).",
    )
    args = parser.parse_args()

    corpus_path: Path = args.corpus
    output_dir: Path = args.output_dir

    if not corpus_path.exists():
        raise FileNotFoundError(f"Corpus not found: {corpus_path}")

    print(f"Loading corpus from {corpus_path}")
    memories, eval_queries, version = load_corpus(corpus_path)
    print(f"Corpus: {len(memories)} memories, {len(eval_queries)} eval queries (schema={version})")

    grid = generate_grid(quick=args.quick)
    print(f"Grid: {len(grid)} configs (quick={args.quick})")

    t_start = time.perf_counter()
    session, note_id_map = setup_session(memories, version=version)
    try:
        # Evaluate default config for the comparison table
        default_metrics = evaluate_config(
            session, _DEFAULT_CONFIG, eval_queries, note_id_map, version=version
        )
        if version == "v2":
            print(
                f"Default config: combined={default_metrics['combined_score']:.4f} "
                f"mrr_exp={default_metrics['mrr_expected']:.4f} "
                f"prec@k={default_metrics['precision_at_k']:.4f}"
            )
        else:
            print(
                f"Default config: recall@10={default_metrics['recall_at_10']:.4f} "
                f"mrr={default_metrics['mrr']:.4f}"
            )

        results = run_grid(session, grid, eval_queries, note_id_map, version=version)
    finally:
        session.close()

    t_elapsed = time.perf_counter() - t_start
    print(f"Grid search complete in {t_elapsed:.1f}s")

    report_filename = "REPORT-v2.md" if version == "v2" else "REPORT.md"
    write_results(
        results,
        output_dir,
        t_total_seconds=t_elapsed,
        n_eval_queries=len(eval_queries),
        default_config_metrics=default_metrics,
        version=version,
        report_filename=report_filename,
    )

    if version == "v2":
        best = max(results, key=lambda r: (r["combined_score"], r.get("mrr_expected", 0.0)))
        combined_scores = [r["combined_score"] for r in results]
        print(
            f"\nBest config: combined={best['combined_score']:.4f} "
            f"mrr_exp={best['mrr_expected']:.4f} prec@k={best['precision_at_k']:.4f} "
            f"(index {best['config_index']})"
        )
        print(
            f"Score range: [{min(combined_scores):.4f}, {max(combined_scores):.4f}] "
            f"(range={max(combined_scores) - min(combined_scores):.4f})"
        )
    else:
        best = max(results, key=lambda r: (r["recall_at_10"], r["mrr"]))
        print(
            f"\nBest config: recall@10={best['recall_at_10']:.4f} mrr={best['mrr']:.4f} "
            f"(index {best['config_index']})"
        )
    print(f"Results written to {output_dir}")


if __name__ == "__main__":
    main()
