#!/usr/bin/env python3
"""Generate deterministic synthetic clustered fvecs fixtures for ANN regression testing.

Produces sift_base.fvecs and sift_query.fvecs in <--out> using only Python stdlib.
Files are byte-identical across runs given the same --seed (fully deterministic).

Usage:
    python3 scripts/perf/gen_synthetic_fvecs.py --out /tmp/synth-fvecs
    python3 scripts/perf/gen_synthetic_fvecs.py --out /tmp/synth-fvecs \\
        --n 50000 --queries 1000 --dim 128 --clusters 64 --sigma 0.08 --seed 42

Output files:
    <out>/sift_base.fvecs   -- --n vectors in fvecs format
    <out>/sift_query.fvecs  -- --queries vectors in fvecs format

fvecs format (per record):
    int32 LE dim, then dim * float32 LE values

The clustering structure (low intrinsic dimension) makes this suitable for
verifying that ANN index build and search behave correctly: well-separated
clusters yield high recall@10, so a recall drop signals structural breakage.
"""

from __future__ import annotations

import argparse
import hashlib
import math
import os
import random
import struct


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Generate deterministic synthetic fvecs fixtures.")
    p.add_argument("--out", required=True, help="Output directory (created if absent)")
    p.add_argument("--n", type=int, default=50000, help="Number of base vectors (default: 50000)")
    p.add_argument(
        "--queries", type=int, default=1000, help="Number of query vectors (default: 1000)"
    )
    p.add_argument("--dim", type=int, default=128, help="Vector dimension (default: 128)")
    p.add_argument(
        "--clusters", type=int, default=64, help="Number of cluster centers (default: 64)"
    )
    p.add_argument(
        "--sigma",
        type=float,
        default=0.08,
        help="Gaussian noise std relative to cluster spacing (default: 0.08). "
        "Lower values produce tighter clusters and higher ANN recall.",
    )
    p.add_argument("--seed", type=int, default=42, help="RNG seed (default: 42)")
    return p.parse_args()


def _gauss_pair(rng: random.Random) -> tuple[float, float]:
    """Box-Muller transform producing two independent N(0,1) samples."""
    while True:
        u = rng.random()
        v = rng.random()
        if u > 0.0:
            break
    mag = math.sqrt(-2.0 * math.log(u))
    return mag * math.cos(2.0 * math.pi * v), mag * math.sin(2.0 * math.pi * v)


def _gauss_vector(rng: random.Random, dim: int) -> list[float]:
    """Return a dim-length list of independent N(0,1) samples."""
    out: list[float] = []
    for _ in range(dim // 2):
        a, b = _gauss_pair(rng)
        out.append(a)
        out.append(b)
    if dim % 2 == 1:
        a, _ = _gauss_pair(rng)
        out.append(a)
    return out


def generate_centers(rng: random.Random, n_clusters: int, dim: int) -> list[list[float]]:
    """Generate cluster centers uniformly in [-1, 1]^dim."""
    centers = []
    for _ in range(n_clusters):
        c = [rng.uniform(-1.0, 1.0) for _ in range(dim)]
        centers.append(c)
    return centers


def generate_vectors(
    rng: random.Random,
    n: int,
    centers: list[list[float]],
    sigma: float,
) -> list[list[float]]:
    """Generate n vectors by assigning each to a cluster center + Gaussian noise."""
    n_clusters = len(centers)
    dim = len(centers[0])
    vectors = []
    for i in range(n):
        cluster_idx = i % n_clusters
        center = centers[cluster_idx]
        noise = _gauss_vector(rng, dim)
        vec = [center[d] + sigma * noise[d] for d in range(dim)]
        vectors.append(vec)
    # Shuffle so cluster assignment isn't trivially sequential in the file.
    rng.shuffle(vectors)
    return vectors


def generate_queries(
    rng: random.Random,
    n_queries: int,
    centers: list[list[float]],
    sigma: float,
) -> list[list[float]]:
    """Generate query vectors: each picks a random center + Gaussian noise."""
    dim = len(centers[0])
    n_clusters = len(centers)
    queries = []
    for _ in range(n_queries):
        cluster_idx = rng.randrange(n_clusters)
        center = centers[cluster_idx]
        noise = _gauss_vector(rng, dim)
        vec = [center[d] + sigma * noise[d] for d in range(dim)]
        queries.append(vec)
    return queries


def write_fvecs(path: str, vectors: list[list[float]], dim: int) -> str:
    """Write vectors to path in fvecs format. Returns hex sha256 of the file."""
    record = struct.pack("<i", dim)  # 4-byte LE int32 dim header per record
    fmt = f"<{dim}f"
    h = hashlib.sha256()
    with open(path, "wb") as f:
        for vec in vectors:
            header = record
            payload = struct.pack(fmt, *vec)
            f.write(header)
            f.write(payload)
            h.update(header)
            h.update(payload)
    return h.hexdigest()


def main() -> None:
    args = parse_args()

    os.makedirs(args.out, exist_ok=True)

    rng = random.Random(args.seed)

    centers = generate_centers(rng, args.clusters, args.dim)
    base_vecs = generate_vectors(rng, args.n, centers, args.sigma)
    query_vecs = generate_queries(rng, args.queries, centers, args.sigma)

    base_path = os.path.join(args.out, "sift_base.fvecs")
    query_path = os.path.join(args.out, "sift_query.fvecs")

    base_sha = write_fvecs(base_path, base_vecs, args.dim)
    query_sha = write_fvecs(query_path, query_vecs, args.dim)

    print(
        f"Generated: n={args.n} queries={args.queries} dim={args.dim} "
        f"clusters={args.clusters} sigma={args.sigma} seed={args.seed}"
    )
    print(f"  base:  {base_path}  sha256={base_sha}")
    print(f"  query: {query_path}  sha256={query_sha}")


if __name__ == "__main__":
    main()
