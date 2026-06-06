# HNSW Distance Metrics

The distance implementations live in `src/distance.rs`.

## Metric Properties (Euclidean / L2)

- `euclidean_nonneg`: $d(x,y) \geq 0$
- `euclidean_self`: $d(x,x) = 0$
- `euclidean_symm`: $d(x,y) = d(y,x)$
- `euclidean_triangle`: $d(x,z) \leq d(x,y) + d(y,z)$

## Cosine Properties

- `cosine_range`: $-1 \leq \cos(x,y) \leq 1$ for unit vectors
- `cosine_not_metric`: cosine does NOT satisfy the triangle inequality

## Dot Product

- `dot_eq_inner`: equivalent to standard inner product

## Distance-Similarity Conversion

- `distanceToSimilarity`: $\text{sim} = \frac{1}{1+d}$ for Euclidean
- `similarity_nonneg`: $\text{similarity} \geq 0$
- `similarity_bounded`: $0 \leq \text{sim} \leq 1$ for $d \geq 0$
