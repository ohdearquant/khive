-- khive.Retrieval.Distance — metric axioms and triangle inequality
-- TODO: Port from khive-internal/platform/retrieval/ (ADR-030 Phase 2)
-- Rust modules: crates/khive-hnsw/src/distance.rs

import Mathlib.Topology.MetricSpace.Basic

namespace khive.Retrieval.Distance

-- Placeholder: distance_nonneg
-- For all vectors u v : ℝⁿ, distance(u, v) ≥ 0
theorem distance_nonneg : True := trivial

-- Placeholder: triangle_inequality
-- For all vectors u v w : ℝⁿ, distance(u, w) ≤ distance(u, v) + distance(v, w)
theorem triangle_inequality : True := trivial

end khive.Retrieval.Distance
