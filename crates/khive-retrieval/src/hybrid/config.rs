//! Hybrid search configuration types.

use std::time::Duration;

use khive_score::DeterministicScore;
use serde::{Deserialize, Serialize};

use khive_fusion::FusionStrategy;

/// Default candidate pool multiplier over top_k.
pub const DEFAULT_POOL_MULTIPLIER: usize = 5;

/// Query for hybrid search.
///
/// Combines text for keyword search and optional embedding for vector search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    /// Text for keyword search (required).
    pub text: String,

    /// Pre-computed embedding for vector search (optional).
    ///
    /// If None, vector search is skipped or caller must provide.
    pub embedding: Option<Vec<f32>>,

    /// Optional filters to apply post-retrieval.
    pub filters: Option<serde_json::Value>,
}

impl Query {
    /// Create a new query with text only (keyword search).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            filters: None,
        }
    }

    /// Create a query with both text and embedding (hybrid search).
    pub fn hybrid(text: impl Into<String>, embedding: Vec<f32>) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            filters: None,
        }
    }

    /// Add filters to the query.
    #[must_use]
    pub fn with_filters(mut self, filters: serde_json::Value) -> Self {
        self.filters = Some(filters);
        self
    }

    /// Check if this query supports vector search.
    pub fn has_embedding(&self) -> bool {
        self.embedding.is_some()
    }
}

/// Raw wire format for [`HybridConfig`], used by `TryFrom` validation.
#[derive(Deserialize)]
struct RawHybridConfig {
    fusion_strategy: FusionStrategy,
    top_k: usize,
    candidate_pool_size: usize,
    min_score: Option<DeterministicScore>,
    vector_weight: f64,
    keyword_weight: f64,
    #[serde(default, with = "crate::timeout::serde_opt_duration")]
    timeout: Option<Duration>,
}

impl TryFrom<RawHybridConfig> for HybridConfig {
    type Error = String;

    fn try_from(raw: RawHybridConfig) -> Result<Self, Self::Error> {
        if !raw.vector_weight.is_finite() {
            return Err(format!(
                "vector_weight must be finite, got {}",
                raw.vector_weight
            ));
        }
        if !raw.keyword_weight.is_finite() {
            return Err(format!(
                "keyword_weight must be finite, got {}",
                raw.keyword_weight
            ));
        }
        Ok(HybridConfig {
            fusion_strategy: raw.fusion_strategy,
            top_k: raw.top_k,
            candidate_pool_size: raw.candidate_pool_size,
            min_score: raw.min_score,
            vector_weight: raw.vector_weight,
            keyword_weight: raw.keyword_weight,
            timeout: raw.timeout,
        })
    }
}

/// Configuration for hybrid search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawHybridConfig")]
pub struct HybridConfig {
    /// Fusion strategy to use (default: RRF with k=60).
    pub fusion_strategy: FusionStrategy,

    /// Number of results to return.
    pub top_k: usize,

    /// Candidates to fetch from each retriever before fusion.
    ///
    /// Should be >= 5 * top_k for quality fusion.
    pub candidate_pool_size: usize,

    /// Minimum score threshold (post-fusion).
    pub min_score: Option<DeterministicScore>,

    /// Weight for vector search results (0.0 to 1.0).
    ///
    /// Only used when fusion_strategy is Weighted.
    pub vector_weight: f64,

    /// Weight for keyword search results (0.0 to 1.0).
    ///
    /// Only used when fusion_strategy is Weighted.
    pub keyword_weight: f64,

    /// Optional timeout for the entire search operation.
    ///
    /// If set, the search will be cancelled if it exceeds this duration,
    /// returning [`crate::error::RetrievalError::QueryTimeout`].
    /// If None, no timeout is applied.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::timeout::serde_opt_duration"
    )]
    pub timeout: Option<Duration>,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50, // 5 * top_k
            min_score: None,
            vector_weight: 0.7,
            keyword_weight: 0.3,
            timeout: None,
        }
    }
}

impl HybridConfig {
    /// Create a new config with specified top_k.
    ///
    /// The candidate pool size is `top_k * DEFAULT_POOL_MULTIPLIER`, saturating
    /// at `usize::MAX` on overflow (rather than wrapping or panicking in debug).
    pub fn new(top_k: usize) -> Self {
        Self {
            top_k,
            candidate_pool_size: top_k.saturating_mul(DEFAULT_POOL_MULTIPLIER),
            ..Default::default()
        }
    }

    /// Set the fusion strategy.
    #[must_use]
    pub fn with_fusion_strategy(mut self, strategy: FusionStrategy) -> Self {
        self.fusion_strategy = strategy;
        self
    }

    /// Set the candidate pool size.
    #[must_use]
    pub fn with_pool_size(mut self, size: usize) -> Self {
        self.candidate_pool_size = size;
        self
    }

    /// Set the minimum score threshold.
    #[must_use]
    pub fn with_min_score(mut self, score: DeterministicScore) -> Self {
        self.min_score = Some(score);
        self
    }

    /// Set weights for weighted fusion (clamped to [0.0, 1.0]). Debug-asserts both weights are finite.
    #[must_use]
    pub fn with_weights(mut self, vector: f64, keyword: f64) -> Self {
        debug_assert!(
            vector.is_finite(),
            "vector weight must be finite, got {vector}"
        );
        debug_assert!(
            keyword.is_finite(),
            "keyword weight must be finite, got {keyword}"
        );
        self.vector_weight = vector.clamp(0.0, 1.0);
        self.keyword_weight = keyword.clamp(0.0, 1.0);
        self
    }

    /// Set the search timeout.
    ///
    /// If the search operation exceeds this duration, it will return
    /// [`crate::error::RetrievalError::QueryTimeout`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Get normalized weights that sum to 1.0.
    ///
    /// If both weights are zero or their sum is non-finite, returns equal weights (0.5, 0.5).
    pub fn normalized_weights(&self) -> (f64, f64) {
        let sum = self.vector_weight + self.keyword_weight;
        if sum <= 0.0 || !sum.is_finite() {
            (0.5, 0.5)
        } else {
            (self.vector_weight / sum, self.keyword_weight / sum)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_text_only() {
        let q = Query::text("hello world");
        assert_eq!(q.text, "hello world");
        assert!(q.embedding.is_none());
        assert!(!q.has_embedding());
    }

    #[test]
    fn test_query_hybrid() {
        let embedding = vec![0.1, 0.2, 0.3];
        let q = Query::hybrid("hello", embedding.clone());
        assert_eq!(q.text, "hello");
        assert_eq!(q.embedding, Some(embedding));
        assert!(q.has_embedding());
    }

    #[test]
    fn test_query_with_filters() {
        let q = Query::text("test").with_filters(serde_json::json!({"type": "memory"}));
        assert!(q.filters.is_some());
    }

    #[test]
    fn test_hybrid_config_default() {
        let config = HybridConfig::default();
        assert_eq!(config.top_k, 10);
        assert_eq!(config.candidate_pool_size, 50);
        assert!(matches!(
            config.fusion_strategy,
            FusionStrategy::Rrf { k: 60 }
        ));
        assert!(config.min_score.is_none());
    }

    #[test]
    fn test_hybrid_config_new() {
        let config = HybridConfig::new(20);
        assert_eq!(config.top_k, 20);
        assert_eq!(config.candidate_pool_size, 100); // 20 * 5
    }

    #[test]
    fn test_hybrid_config_builder() {
        let config = HybridConfig::new(10)
            .with_fusion_strategy(FusionStrategy::union())
            .with_pool_size(200)
            .with_weights(0.6, 0.4);

        assert_eq!(config.top_k, 10);
        assert_eq!(config.candidate_pool_size, 200);
        assert!(matches!(config.fusion_strategy, FusionStrategy::Union));
        assert_eq!(config.vector_weight, 0.6);
        assert_eq!(config.keyword_weight, 0.4);
    }

    #[test]
    fn test_normalized_weights() {
        let config = HybridConfig::default();
        let (v, k) = config.normalized_weights();
        assert!((v - 0.7).abs() < 0.01);
        assert!((k - 0.3).abs() < 0.01);

        // Zero weights -> equal
        let config = HybridConfig::default().with_weights(0.0, 0.0);
        let (v, k) = config.normalized_weights();
        assert!((v - 0.5).abs() < 0.01);
        assert!((k - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_weight_clamping() {
        let config = HybridConfig::default().with_weights(1.5, -0.5);
        assert_eq!(config.vector_weight, 1.0);
        assert_eq!(config.keyword_weight, 0.0);
    }

    /// JSON has no NaN token; the parser rejects the literal before `TryFrom` is ever
    /// reached. This documents that trivial case but is NOT the real regression guard —
    /// see `test_try_from_rejects_nan_vector_weight` below for the genuine boundary test.
    #[test]
    fn test_serde_json_rejects_nan_literal_vector_weight() {
        let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": NaN,
            "keyword_weight": 0.3
        }"#;
        let result: Result<HybridConfig, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "JSON literal NaN must be rejected by the parser"
        );
    }

    /// Same caveat as above: `Infinity` is not a valid JSON token either.
    #[test]
    fn test_serde_json_rejects_infinity_literal_vector_weight() {
        let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": Infinity,
            "keyword_weight": 0.3
        }"#;
        let result: Result<HybridConfig, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "JSON literal Infinity must be rejected by the parser"
        );
    }

    /// `serde_json` refuses any JSON number literal that overflows `f64` (e.g. `1e400`)
    /// with its own "number out of range" parse error, regardless of any custom
    /// validation — verified empirically: this also occurs for a bare `f64` field with no
    /// `TryFrom` involved at all. So it cannot be used to prove `TryFrom` is wired up; it's
    /// omitted here to avoid a misleading assertion. The genuine regression guard for both
    /// NaN and infinity is `RawHybridConfig` constructed directly below, which is exactly
    /// the value `TryFrom<RawHybridConfig>` receives once `serde` finishes parsing the raw
    /// wire form — see `test_try_from_rejects_*` below.
    ///
    /// `serde_json` also cannot encode a literal NaN value at all
    /// (`serde_json::Number::from_f64(f64::NAN)` returns `None`), so the only way to
    /// exercise `TryFrom<RawHybridConfig>` with an actual non-finite value is to construct
    /// the raw struct directly. Without the fix, `RawHybridConfig`/`TryFrom` do not exist
    /// and these tests would not compile; with the old plain-derive `Deserialize`, this
    /// exact input would have deserialized successfully (uncaught in release, since the
    /// only prior guard was a `debug_assert!` in the unrelated `with_weights` builder).
    #[test]
    fn test_try_from_rejects_nan_vector_weight() {
        let raw = RawHybridConfig {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50,
            min_score: None,
            vector_weight: f64::NAN,
            keyword_weight: 0.3,
            timeout: None,
        };
        let result = HybridConfig::try_from(raw);
        assert!(
            result.is_err(),
            "NaN vector_weight must be rejected via TryFrom"
        );
    }

    #[test]
    fn test_try_from_rejects_infinite_vector_weight() {
        let raw = RawHybridConfig {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50,
            min_score: None,
            vector_weight: f64::INFINITY,
            keyword_weight: 0.3,
            timeout: None,
        };
        let result = HybridConfig::try_from(raw);
        assert!(
            result.is_err(),
            "+Infinity vector_weight must be rejected via TryFrom"
        );
    }

    #[test]
    fn test_try_from_rejects_nan_keyword_weight() {
        let raw = RawHybridConfig {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50,
            min_score: None,
            vector_weight: 0.7,
            keyword_weight: f64::NAN,
            timeout: None,
        };
        let result = HybridConfig::try_from(raw);
        assert!(
            result.is_err(),
            "NaN keyword_weight must be rejected via TryFrom"
        );
    }

    #[test]
    fn test_try_from_rejects_negative_infinity_keyword_weight() {
        let raw = RawHybridConfig {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50,
            min_score: None,
            vector_weight: 0.7,
            keyword_weight: f64::NEG_INFINITY,
            timeout: None,
        };
        let result = HybridConfig::try_from(raw);
        assert!(
            result.is_err(),
            "-Infinity keyword_weight must be rejected via TryFrom"
        );
    }

    /// Positive control: a valid finite config still deserializes correctly through the
    /// `TryFrom` boundary — the fix must not reject legitimate input.
    #[test]
    fn test_serde_accepts_valid_finite_config() {
        let json = r#"{
            "fusion_strategy": {"rrf": {"k": 60}},
            "top_k": 10,
            "candidate_pool_size": 50,
            "min_score": null,
            "vector_weight": 0.7,
            "keyword_weight": 0.3
        }"#;
        let config: HybridConfig = serde_json::from_str(json).expect("valid config");
        assert_eq!(config.top_k, 10);
        assert_eq!(config.candidate_pool_size, 50);
        assert_eq!(config.vector_weight, 0.7);
        assert_eq!(config.keyword_weight, 0.3);
    }

    #[test]
    fn test_serde_roundtrip_preserves_default_config() {
        let config = HybridConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: HybridConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.vector_weight, config.vector_weight);
        assert_eq!(restored.keyword_weight, config.keyword_weight);
        assert_eq!(restored.top_k, config.top_k);
    }
}
