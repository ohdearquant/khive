//! Serializable evidence for an already resolved retrieval execution.
//!
//! This additive foundation follows Proposed ADR-031 Amendment 5 Decision C. It does
//! not change an existing response, resolve defaults, validate request/configuration
//! input, or activate a retrieval path. Producers validate the plan before work and
//! report observations in canonical configured-engine order. See `docs/api/evidence.md`.

use khive_fusion::FusionStrategy;
use serde::Serialize;

/// Version of this proposed two-stage strategy evidence representation.
pub const RETRIEVAL_STRATEGY_VERSION: u32 = 1;

/// The resolved engine and hybrid stages, composed from the existing strategies.
///
/// This is output evidence, not the unresolved request form. Weighted engine fusion
/// carries the resolved positional weights in configured-engine order. `None` is
/// serialized as null for a keyword-only plan. No default or fallback is supplied.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetrievalStrategy {
    /// Fusion over the selected ordered vector-engine arms, absent for keyword only.
    pub engine_fusion: Option<FusionStrategy>,
    /// Fusion over exactly `[combined_vector, text]`.
    pub hybrid_fusion: FusionStrategy,
}

/// Where the outermost entry point resolved the selected strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategySource {
    /// An explicit request strategy.
    Request,
    /// The validated configured default.
    Config,
}

/// Execution disposition; running with zero hits is distinct from every other case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ArmOutcome {
    /// The arm ran. Its returned candidate count may be zero.
    Ran,
    /// The plan or a request boundary skipped this arm.
    Skipped {
        /// Producer-supplied explanation for skipping the arm.
        reason: String,
    },
    /// The arm could not run because its backend or provider was unavailable.
    Unavailable {
        /// Producer-supplied explanation of the unavailable dependency.
        reason: String,
    },
    /// The arm ran and failed.
    Failed {
        /// Producer-supplied failure explanation, without a raw query vector.
        reason: String,
    },
}

/// Candidate accounting and disposition for one engine or the text arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArmEvidence {
    /// Candidate count requested from the arm, before its downstream final limit.
    pub requested_candidates: usize,
    /// Candidate count actually returned by the arm, including a measured zero.
    pub returned_candidates: usize,
    /// Whether the arm ran, was skipped, was unavailable, or failed, with its reason.
    pub outcome: ArmOutcome,
}

/// One selected engine's resolved weight and execution evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EngineEvidence {
    /// Canonical engine name, never a physical vector-table alias.
    pub name: String,
    /// Effective weight; explicitly unweighted engine RRF reports one for each engine.
    pub effective_weight: f64,
    /// Candidate accounting and execution disposition for this engine.
    pub arm: ArmEvidence,
}

/// Meaning of a fusion stage's ordering score, not a probability or raw cosine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalScoreKind {
    /// Unweighted reciprocal rank fusion.
    Rrf,
    /// Weighted reciprocal rank fusion.
    WeightedRrf,
    /// Normalized linear score blending.
    Weighted,
    /// Maximum-score union, which is not scale independent.
    Union,
    /// Vector-stage score retained without text fusion.
    Vector,
    /// Text-stage score retained without vector fusion.
    Keyword,
    /// A custom executor's explicitly declared score kind.
    Custom {
        /// Score-kind label declared by the executor, not an inferred fallback.
        name: String,
    },
}

/// Plan and observations carried on populated and zero-result responses alike.
///
/// This is a serialization model, not a validation boundary. Producers must supply
/// finite positive effective weights and consistent plan/count/participation facts.
/// In particular, a used engine completed successfully, including one returning zero hits.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetrievalEvidence {
    /// The resolved stages; no pack-local default is reconstructed during serialization.
    pub strategy: RetrievalStrategy,
    /// Representation version, normally [`RETRIEVAL_STRATEGY_VERSION`].
    pub strategy_version: u32,
    /// Whether the outermost entry resolved a request or configured strategy.
    pub strategy_source: StrategySource,
    /// Explicit requested names in canonical order; null means the subset was omitted.
    /// An explicitly empty subset remains an empty array.
    pub requested_engines: Option<Vec<String>>,
    /// Applicable selected engines in canonical configured order.
    pub selected_engines: Vec<String>,
    /// Successfully completed engines in canonical order, even with zero returned hits.
    pub used_engines: Vec<String>,
    /// One record per selected engine in the same canonical order.
    pub engines: Vec<EngineEvidence>,
    /// Text participation, including skipped/failed reasons and candidate counts.
    pub text: ArmEvidence,
    /// Whether the observed execution was degraded under the producer's policy.
    pub degraded: bool,
    /// Engine-stage score kind, absent when the plan has no vector stage.
    pub engine_score_kind: Option<RetrievalScoreKind>,
    /// Hybrid-stage score kind, without relabelling the score as raw similarity.
    pub hybrid_score_kind: RetrievalScoreKind,
}

/// Successful result values plus the resolved retrieval plan and execution evidence.
///
/// `Hit` is the caller's existing serializable wire hit. Empty results retain all
/// evidence. Diagnostics have no field for a query vector; producers remain
/// responsible for keeping vector contents out of free-form reasons and hit data.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetrievalEnvelope<Hit> {
    /// Result values in the producer's final order.
    pub results: Vec<Hit>,
    /// Resolved plan and execution observations, even when results are empty.
    pub retrieval: RetrievalEvidence,
}
