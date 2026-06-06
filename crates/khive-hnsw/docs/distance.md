# HNSW Distance Metrics

The distance implementations live in `src/distance.rs`.

## Metric Properties (Euclidean / L2)

- `euclidean_nonneg`: d(x,y) ≥ 0
- `euclidean_self`: d(x,x) = 0
- `euclidean_symm`: d(x,y) = d(y,x)
- `euclidean_triangle`: d(x,z) ≤ d(x,y) + d(y,z)

## Cosine Properties

- `cosine_range`: -1 ≤ cos(x,y) ≤ 1 for unit vectors
- `cosine_not_metric`: cosine does NOT satisfy the triangle inequality

## Dot Product

- `dot_eq_inner`: equivalent to standard inner product

## Distance-Similarity Conversion

- `distanceToSimilarity`: sim = 1/(1+d) for Euclidean
- `similarity_nonneg`: similarity ≥ 0
- `similarity_bounded`: 0 ≤ sim ≤ 1 for d ≥ 0
