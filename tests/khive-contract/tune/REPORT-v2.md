# Historical report — pre-salience rename (2026-05-25)
# Param-Tuning Grid Search Report

- **Date**: 2026-05-25
- **Corpus version**: v2
- **Grid size**: 232 configs
- **Eval queries**: 48
- **Total runtime**: 6.4s
- **Mode**: FTS-only (no_embed=True)

## Winning Config (highest combined_score)

| Metric | Value |
|--------|-------|
| combined_score | 0.1302 |
| mrr_expected | 0.1667 |
| precision_at_k | 0.1562 |
| exclusion_penalty | 0.0000 |
| recall_at_10 | 0.1562 |
| mrr (v1) | 0.1667 |
| mean latency | 0.5ms |
| config_index | 0 |

Parameters: `rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=20) decay=exponential hl=14.0 ms=0.0`

## Default vs Tuned Comparison

| Metric | Default config | Tuned config | Delta |
|--------|---------------|-------------|-------|
| combined_score | 0.1302 | 0.1302 | +0.0000 |
| mrr_expected | 0.1667 | 0.1667 | +0.0000 |
| precision_at_k | 0.1562 | 0.1562 | +0.0000 |
| exclusion_penalty | 0.0000 | 0.0000 | +0.0000 |
| recall_at_10 | 0.1562 | 0.1562 | +0.0000 |
| mean latency | 0.6ms | 0.5ms | -0.0ms |

Default config: relevance=0.70 importance=0.20 temporal=0.10 candidate_multiplier=20 fuse=rrf(k=60) decay=exponential half_life=30.0

## Discrimination Analysis (v2 corpus)

| Metric | Distinct values | Min | Max | Range |
|--------|-----------------|-----|-----|-------|
| combined_score | 2 | 0.0500 | 0.1302 | 0.0802 |
| mrr_expected | 2 | 0.0625 | 0.1667 | 0.1042 |
| precision_at_k | 2 | 0.0625 | 0.1562 | 0.0937 |

A non-flat landscape requires combined_score range > 0.05 across configs.

## Top 10 by combined_score

| idx | combined | mrr_exp | prec@k | excl_pen | latency | config |
|-----|---------|---------|--------|----------|---------|--------|
|    0 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.5ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=20) decay=exponential hl=14.0 ms=0.0 |
|    1 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=60) decay=exponential hl=30.0 ms=0.0 |
|    2 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=100) decay=exponential hl=60.0 ms=0.0 |
|    3 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(1.0/0.0) decay=hyperbolic hl=14.0 ms=0.0 |
|    4 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.75/0.25) decay=hyperbolic hl=30.0 ms=0.0 |
|    5 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.5ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.5/0.5) decay=hyperbolic hl=60.0 ms=0.0 |
|    6 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.25/0.75) decay=none hl=14.0 ms=0.0 |
|    7 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.0/1.0) decay=none hl=30.0 ms=0.0 |
|    8 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=rrf(k=20) decay=none hl=60.0 ms=0.0 |
|    9 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=rrf(k=100) decay=exponential hl=14.0 ms=0.0 |

## Top 10 by MRR

| idx | combined | mrr_exp | prec@k | excl_pen | latency | config |
|-----|---------|---------|--------|----------|---------|--------|
|    0 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.5ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=20) decay=exponential hl=14.0 ms=0.0 |
|    1 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=60) decay=exponential hl=30.0 ms=0.0 |
|    2 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=rrf(k=100) decay=exponential hl=60.0 ms=0.0 |
|    3 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(1.0/0.0) decay=hyperbolic hl=14.0 ms=0.0 |
|    4 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.75/0.25) decay=hyperbolic hl=30.0 ms=0.0 |
|    5 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.5ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.5/0.5) decay=hyperbolic hl=60.0 ms=0.0 |
|    6 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.25/0.75) decay=none hl=14.0 ms=0.0 |
|    7 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=10 fuse=weighted(0.0/1.0) decay=none hl=30.0 ms=0.0 |
|    8 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=rrf(k=20) decay=none hl=60.0 ms=0.0 |
|    9 | 0.1302 | 0.1667 | 0.1562 | 0.0000 | 0.6ms | rel=0.7 imp=0.2 tmp=0.1 cand=20 fuse=rrf(k=100) decay=exponential hl=14.0 ms=0.0 |
