# Historical report — pre-salience rename (2026-05-25)
# Param-Tuning Grid Search Report

- **Date**: 2026-05-25
- **Grid size**: 116 configs
- **Eval queries**: 20
- **Total runtime**: 0.7s
- **Mode**: FTS-only (no_embed=True)

## Winning Config (highest recall@10)

| Metric | Value |
|--------|-------|
| recall@10 | 0.9333 |
| MRR | 0.9500 |
| mean latency | 0.3ms |
| config_index | 3 |

Parameters: `rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(1.0/0.0) decay=hyperbolic hl=14.0`

## Default vs Tuned Comparison

| Metric | Default config | Tuned config | Delta |
|--------|---------------|-------------|-------|
| recall@10 | 0.9333 | 0.9333 | +0.0000 |
| MRR | 0.9250 | 0.9500 | +0.0250 |
| mean latency | 0.3ms | 0.3ms | -0.0ms |

Default config: relevance=0.70 importance=0.20 temporal=0.10 candidate_multiplier=20 fuse=rrf(k=60) decay=exponential half_life=30.0

## Flat Optimization Landscape

All 116 configs achieve **identical** recall@10 = 0.9333. MRR has exactly two values:
0.925 (all RRF + vector-only weighted configs, 58 total) and 0.950 (all other weighted
configs, 58 total). The split is determined entirely by fusion strategy — `relevance_weight`,
`importance_weight`, `temporal_weight`, `candidate_multiplier`, `decay_model`, and
`temporal_half_life_days` have **zero measurable effect** on either metric.

**Root cause**: The synthetic corpus uses short exact-keyword queries against FTS5 (AND-logic).
Every relevant memory contains the query terms, so FTS5 trivially returns them regardless of
scoring parameters. A harder eval set (synonyms, cross-domain reasoning, partial matches) is
needed to discriminate non-fusion parameters.

The three committed default changes (`half_life 30→14`, `decay exp→hyp`, `multiplier 20→10`)
are benign — they pass validation and lie within sensible ranges — but they are not empirically
distinguished from the old defaults by this grid search.

## Top 10 by recall@10

| idx | recall@10 | mrr | latency | config |
|-----|-----------|-----|---------|--------|
|    3 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(1.0/0.0) decay=hyperbolic hl=14.0 |
|    4 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.75/0.25) decay=hyperbolic hl=30.0 |
|    5 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.5/0.5) decay=hyperbolic hl=60.0 |
|    6 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.25/0.75) decay=none hl=14.0 |
|   10 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(1.0/0.0) decay=exponential hl=30.0 |
|   11 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.75/0.25) decay=exponential hl=60.0 |
|   12 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.5/0.5) decay=hyperbolic hl=14.0 |
|   13 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.25/0.75) decay=hyperbolic hl=30.0 |
|   18 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=40 fuse=weighted(0.75/0.25) decay=exponential hl=14.0 |
|   19 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=40 fuse=weighted(0.5/0.5) decay=exponential hl=30.0 |

## Top 10 by MRR

| idx | recall@10 | mrr | latency | config |
|-----|-----------|-----|---------|--------|
|    3 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(1.0/0.0) decay=hyperbolic hl=14.0 |
|    4 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.75/0.25) decay=hyperbolic hl=30.0 |
|    5 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.5/0.5) decay=hyperbolic hl=60.0 |
|    6 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.25/0.75) decay=none hl=14.0 |
|   10 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(1.0/0.0) decay=exponential hl=30.0 |
|   11 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.75/0.25) decay=exponential hl=60.0 |
|   12 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.5/0.5) decay=hyperbolic hl=14.0 |
|   13 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=weighted(0.25/0.75) decay=hyperbolic hl=30.0 |
|   18 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=40 fuse=weighted(0.75/0.25) decay=exponential hl=14.0 |
|   19 | 0.9333 | 0.9500 | 0.3ms | rel=0.7 imp=0.2 tmp=0.1 cand=40 fuse=weighted(0.5/0.5) decay=exponential hl=30.0 |
