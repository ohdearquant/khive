//! Optional pack capability for returning owned context slices.
//!
//! This module defines the contributor boundary only. Discovery does not run contributors;
//! composition, timeouts, budget allocation and profile resolution belong to callers.

use async_trait::async_trait;
use khive_storage::Direction;
use serde::Serialize;
use serde_json::Value;

use crate::{NamespaceToken, RuntimeError};

/// Owned input to a context contributor, without parsing, defaults or clamping.
#[derive(Clone, Debug)]
pub struct ContextRequest {
    pub query: Option<String>,
    pub entity_ids: Vec<String>,
    pub consumer_kind: String,
    /// Advisory budget for the total assembly, not an allocation to this contributor.
    pub budget_hint: usize,
    pub hops: u8,
    pub fanout: u8,
    pub direction: Direction,
    pub relations: Vec<String>,
}

/// Meaning of a score within its source partition; scores are not cross-pack comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreSemantics {
    DecayWeighted,
    Rerank,
    GraphProximity,
    Relevance,
    None,
}

/// An owned context record with static pack attribution and an unnormalized local score.
#[derive(Clone, Debug, Serialize)]
pub struct ContextSlice {
    pub source_pack: &'static str,
    pub kind: String,
    pub id: String,
    pub content: Value,
    pub score: Option<f64>,
    pub score_semantics: ScoreSemantics,
}

/// Optional object-safe context source implemented by a pack.
///
/// Implementations must honor the supplied namespace token, return owned records, and release
/// all read snapshots before returning. `budget_hint` is advisory for the entire assembly;
/// it is not a per-contributor allocation. Returned scores keep their source-local semantics.
#[async_trait]
pub trait ContextContributor: Send + Sync {
    /// Stable pack metadata used to attribute this source.
    fn source_pack(&self) -> &'static str;

    async fn contribute(
        &self,
        req: &ContextRequest,
        token: &NamespaceToken,
    ) -> Result<Vec<ContextSlice>, RuntimeError>;
}
