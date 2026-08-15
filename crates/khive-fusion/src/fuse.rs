//! Main fusion entry point.

use khive_score::DeterministicScore;
use std::hash::Hash;

use super::registry::{FusionRegistry, UnknownFusionStrategy};
use super::rrf::reciprocal_rank_fusion;
use super::strategy::FusionStrategy;
use super::union::union_fusion;
use super::weighted::weighted_fusion;

/// Fuse ranked sources and retain at most `top_k` results.
///
/// RRF, weighted, and union results sort by score then ID; pass-through modes preserve source
/// order. Custom strategies return [`FuseError::CustomRequiresRuntime`]. See
/// `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn fuse<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    top_k: usize,
) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
    if sources.is_empty() || top_k == 0 {
        return Ok(Vec::new());
    }

    let fused = match strategy {
        FusionStrategy::Rrf { k } => reciprocal_rank_fusion(sources, *k),
        FusionStrategy::Weighted { weights } => weighted_fusion(sources, weights),
        FusionStrategy::Union => union_fusion(sources),
        FusionStrategy::VectorOnly => passthrough_source(sources, 0),
        FusionStrategy::KeywordOnly => passthrough_source(sources, 1),
        FusionStrategy::Custom { name, .. } => {
            return Err(FuseError::CustomRequiresRuntime(name.clone()));
        }
    };

    Ok(fused.into_iter().take(top_k).collect())
}

/// Like [`fuse`], but dispatches `FusionStrategy::Custom` through `registry`
/// by name instead of returning [`FuseError::CustomRequiresRuntime`]. Every
/// other strategy behaves identically to [`fuse`] -- registering a custom
/// strategy never changes the default (non-Custom) dispatch path.
///
/// An unregistered `name` fails closed with
/// [`FuseError::UnknownCustomStrategy`] rather than silently falling back to
/// RRF or any other default.
pub fn fuse_with_registry<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    top_k: usize,
    registry: &FusionRegistry<Id>,
) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
    if sources.is_empty() || top_k == 0 {
        return Ok(Vec::new());
    }

    let FusionStrategy::Custom { name, params } = strategy else {
        return fuse(sources, strategy, top_k);
    };

    let fused = registry
        .dispatch(name, sources, params)
        .map_err(|UnknownFusionStrategy(name)| FuseError::UnknownCustomStrategy(name))?;

    Ok(fused.into_iter().take(top_k).collect())
}

/// Select a single source by index, treating a lone source as authoritative
/// regardless of the requested index (e.g. vector-only search with no keyword source).
fn passthrough_source<Id>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    source_index: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.len() == 1 {
        return sources.into_iter().next().unwrap_or_default();
    }

    sources.into_iter().nth(source_index).unwrap_or_default()
}

/// Error from the [`fuse`] entry point.
#[derive(Debug, Clone, PartialEq)]
pub enum FuseError {
    /// Custom strategies must be dispatched through the runtime registry.
    CustomRequiresRuntime(String),
    /// [`fuse_with_registry`] was called with a `Custom` name that has no
    /// registration in the supplied [`FusionRegistry`].
    UnknownCustomStrategy(String),
}

impl std::fmt::Display for FuseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CustomRequiresRuntime(name) => {
                write!(
                    f,
                    "custom strategy '{}' requires runtime FusionRegistry dispatch",
                    name
                )
            }
            Self::UnknownCustomStrategy(name) => {
                write!(f, "no fusion strategy registered under name '{}'", name)
            }
        }
    }
}

impl std::error::Error for FuseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
            .collect()
    }

    #[test]
    fn test_fuse_rrf_strategy() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_weighted_strategy() {
        let source = make_results(vec![("doc_a", 1.0)]);
        let fused = fuse(vec![source], &FusionStrategy::weighted(vec![1.0]), 10).unwrap();

        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_fuse_union_strategy() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::union(), 10).unwrap();

        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_fuse_top_k_truncation() {
        let source = make_results(vec![
            ("doc_a", 0.9),
            ("doc_b", 0.8),
            ("doc_c", 0.7),
            ("doc_d", 0.6),
            ("doc_e", 0.5),
        ]);

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 3).unwrap();

        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
        assert_eq!(fused[2].0, "doc_c");
    }

    #[test]
    fn test_fuse_top_k_zero() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 0).unwrap();

        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> =
            fuse(vec![], &FusionStrategy::rrf(), 10).unwrap();
        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_top_k_larger_than_results() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 100).unwrap();

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_with_string_ids() {
        let source: Vec<(String, DeterministicScore)> = vec![
            ("doc_a".to_string(), DeterministicScore::from_f64(0.9)),
            ("doc_b".to_string(), DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "doc_a");
    }

    #[test]
    fn test_fuse_with_integer_ids() {
        let source: Vec<(u64, DeterministicScore)> = vec![
            (1, DeterministicScore::from_f64(0.9)),
            (2, DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, 1);
    }

    #[test]
    fn vector_only_two_sources_returns_only_vector_source() {
        let vector = make_results(vec![("vec_only", 0.9)]);
        let keyword = make_results(vec![("kw_only", 1.0)]);
        let out = fuse(vec![vector, keyword], &FusionStrategy::VectorOnly, 10).unwrap();
        let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["vec_only"]);
    }

    #[test]
    fn keyword_only_two_sources_returns_only_keyword_source() {
        let vector = make_results(vec![("vec_only", 0.9)]);
        let keyword = make_results(vec![("kw_only", 1.0)]);
        let out = fuse(vec![vector, keyword], &FusionStrategy::KeywordOnly, 10).unwrap();
        let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["kw_only"]);
    }

    #[test]
    fn test_fuse_custom_returns_error() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let strategy =
            FusionStrategy::try_custom("decay_weighted".to_string(), serde_json::json!({}))
                .unwrap();
        let result = fuse(vec![source], &strategy, 10);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            FuseError::CustomRequiresRuntime("decay_weighted".to_string())
        );
    }

    fn reverse_order_fusion(
        sources: Vec<Vec<(&'static str, DeterministicScore)>>,
        _params: &serde_json::Value,
    ) -> Vec<(&'static str, DeterministicScore)> {
        let mut flat: Vec<_> = sources.into_iter().flatten().collect();
        flat.reverse();
        flat
    }

    #[test]
    fn fuse_with_registry_dispatches_custom_and_differs_from_rrf() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.5), ("doc_c", 0.1)]);
        let mut registry: FusionRegistry<&'static str> = FusionRegistry::new();
        registry.register("reverse", reverse_order_fusion);
        let strategy =
            FusionStrategy::try_custom("reverse".to_string(), serde_json::Value::Null).unwrap();

        let custom = fuse_with_registry(vec![source.clone()], &strategy, 10, &registry).unwrap();
        let rrf = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        let custom_ids: Vec<_> = custom.iter().map(|(id, _)| *id).collect();
        let rrf_ids: Vec<_> = rrf.iter().map(|(id, _)| *id).collect();
        assert_eq!(custom_ids, vec!["doc_c", "doc_b", "doc_a"]);
        assert_eq!(rrf_ids, vec!["doc_a", "doc_b", "doc_c"]);
        assert_ne!(
            custom_ids, rrf_ids,
            "custom and RRF must yield different orderings on this fixture"
        );
    }

    #[test]
    fn fuse_with_registry_unknown_name_fails_closed() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let registry: FusionRegistry<&'static str> = FusionRegistry::new();
        let strategy =
            FusionStrategy::try_custom("nonexistent".to_string(), serde_json::Value::Null).unwrap();

        let result = fuse_with_registry(vec![source], &strategy, 10, &registry);
        assert_eq!(
            result.unwrap_err(),
            FuseError::UnknownCustomStrategy("nonexistent".to_string())
        );
    }

    #[test]
    fn fuse_with_registry_default_path_unaffected_by_registered_custom() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.5)]);
        let mut registry: FusionRegistry<&'static str> = FusionRegistry::new();
        registry.register("reverse", reverse_order_fusion);

        let via_registry =
            fuse_with_registry(vec![source.clone()], &FusionStrategy::rrf(), 10, &registry)
                .unwrap();
        let via_default = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();
        assert_eq!(via_registry, via_default);
    }
}
