//! Moodboard policy for the shared bounded ranked-prefix controller.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use khive_fusion::union_fusion;
use khive_retrieval::{
    materialize_ranked_prefix, DropReason, MaterializationDecision, MaterializationError,
    MaterializationLimits, MaterializedItem, MaterializedPrefix, RankedCandidate,
};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::blob::ContentRef;
use khive_storage::types::VectorSearchHit;
use khive_storage::BlobStore;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::handlers::{candidate_limit, MAX_TOP_K};

pub(crate) async fn materialize_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    raw_hits: Vec<VectorSearchHit>,
    top_k: u32,
) -> Result<Vec<Value>, RuntimeError> {
    let materialized = materialize_hits_with_diagnostics(
        runtime,
        token,
        blob_store,
        query_asset_id,
        raw_hits,
        top_k,
    )
    .await?;
    Ok(materialized
        .accepted
        .into_iter()
        .map(|item| item.output)
        .collect())
}

#[cfg(test)]
pub(crate) fn validated_cosine_score(hit: &VectorSearchHit) -> Result<f64, RuntimeError> {
    validated_cosine_score_value(hit.subject_id, hit.score)
}

fn validated_cosine_score_value(
    subject_id: Uuid,
    deterministic_score: DeterministicScore,
) -> Result<f64, RuntimeError> {
    let score = deterministic_score.to_f64();
    if !score.is_finite() || !(-1.0..=1.0).contains(&score) {
        return Err(RuntimeError::Internal(format!(
            "moodboard vector backend returned invalid cosine score {score} for {} (expected finite [-1,1])",
            subject_id
        )));
    }
    Ok(score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoodboardDropReason {
    SelfHit,
    StaleEntity,
    OutsideVisibleScope,
    WrongKind,
    WrongSubtype,
    MissingContentAttachment,
    MalformedContentRef,
    MissingBlob,
}

impl DropReason for MoodboardDropReason {
    const ALL: &'static [Self] = &[
        Self::SelfHit,
        Self::StaleEntity,
        Self::OutsideVisibleScope,
        Self::WrongKind,
        Self::WrongSubtype,
        Self::MissingContentAttachment,
        Self::MalformedContentRef,
        Self::MissingBlob,
    ];

    fn ordinal(self) -> usize {
        match self {
            Self::SelfHit => 0,
            Self::StaleEntity => 1,
            Self::OutsideVisibleScope => 2,
            Self::WrongKind => 3,
            Self::WrongSubtype => 4,
            Self::MissingContentAttachment => 5,
            Self::MalformedContentRef => 6,
            Self::MissingBlob => 7,
        }
    }
}

#[derive(Debug)]
enum MoodboardCandidateRow {
    Drop(MoodboardDropReason),
    Keep {
        asset_id: Uuid,
        name: String,
        content_ref: ContentRef,
    },
}

type MoodboardMaterializedHits =
    MaterializedPrefix<Uuid, DeterministicScore, Value, MoodboardDropReason>;

/// Validates one candidate's ordering score; the error aborts the request.
type ScoreValidator = fn(Uuid, DeterministicScore) -> Result<f64, RuntimeError>;

async fn materialize_hits_with_diagnostics(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    raw_hits: Vec<VectorSearchHit>,
    top_k: u32,
) -> Result<MoodboardMaterializedHits, RuntimeError> {
    let candidates = raw_hits
        .into_iter()
        .map(|hit| RankedCandidate {
            key: hit.subject_id,
            score: hit.score,
        })
        .collect();
    materialize_ranked_candidates(
        runtime,
        token,
        blob_store,
        query_asset_id,
        candidates,
        top_k,
        validated_cosine_score_value,
    )
    .await
}

/// Bounded ranked-prefix materialization shared by the cosine and fused modes.
///
/// Candidates must already be in final order (score descending, id ascending).
/// `validate_score` decides which score range is legal for the caller's list.
async fn materialize_ranked_candidates(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    candidates: Vec<RankedCandidate<Uuid, DeterministicScore>>,
    top_k: u32,
    validate_score: ScoreValidator,
) -> Result<MoodboardMaterializedHits, RuntimeError> {
    let loader_batch_size = NonZeroUsize::new(1).expect("one is non-zero");
    let max_top_k = usize::try_from(MAX_TOP_K).expect("moodboard top_k ceiling fits usize");
    let max_candidates = usize::try_from(candidate_limit(MAX_TOP_K))
        .expect("moodboard candidate ceiling fits usize");
    let limits = MaterializationLimits::try_new(
        max_candidates,
        loader_batch_size,
        max_top_k,
        max_candidates,
    )
    .map_err(|error| {
        RuntimeError::Internal(format!(
            "moodboard materialization limits violate the shared v1 envelope: {error}"
        ))
    })?;
    let authorized_namespaces: BTreeSet<String> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str().to_string())
        .collect();
    let materialized = materialize_ranked_prefix(
        candidates,
        top_k as usize,
        loader_batch_size,
        limits,
        |candidate| (Reverse(candidate.score), candidate.key),
        |candidate| {
            validate_score(candidate.key, candidate.score)?;
            Ok(())
        },
        |keys| {
            load_moodboard_candidate_batch(
                runtime,
                token,
                blob_store,
                query_asset_id,
                &authorized_namespaces,
                keys,
            )
        },
        |_, row| match row {
            Some(MoodboardCandidateRow::Drop(reason)) => MaterializationDecision::Drop(reason),
            Some(MoodboardCandidateRow::Keep {
                asset_id,
                name,
                content_ref,
            }) => MaterializationDecision::Keep((asset_id, name, content_ref)),
            None => MaterializationDecision::Drop(MoodboardDropReason::StaleEntity),
        },
    )
    .await
    .map_err(map_moodboard_materialization_error)?;

    let accepted = materialized
        .accepted
        .into_iter()
        .map(|item| {
            let (asset_id, name, content_ref) = item.output;
            let output = json!({
                "asset_id": asset_id.to_string(),
                "score": item.candidate.score.to_f64(),
                "rank": item.rank,
                "name": name,
                "content_ref": content_ref.to_string(),
            });
            MaterializedItem {
                candidate: item.candidate,
                rank: item.rank,
                output,
            }
        })
        .collect();

    Ok(MaterializedPrefix {
        accepted,
        drop_counts: materialized.drop_counts,
        diagnostic_details: materialized.diagnostic_details,
        diagnostics_truncated: materialized.diagnostics_truncated,
    })
}

/// Fused scores are reciprocal-rank sums, never cosines: they must be finite
/// and non-negative.
pub(crate) fn validated_fused_score_value(
    subject_id: Uuid,
    deterministic_score: DeterministicScore,
) -> Result<f64, RuntimeError> {
    let score = deterministic_score.to_f64();
    if !score.is_finite() || score < 0.0 {
        return Err(RuntimeError::Internal(format!(
            "moodboard fused rerank produced invalid score {score} for {subject_id} (expected finite >= 0)"
        )));
    }
    Ok(score)
}

/// Position of one asset in a ranked input list: 1-based rank and the list's
/// own score for it.
#[derive(Clone, Copy)]
pub(crate) struct ListPosition {
    pub(crate) rank: u32,
    pub(crate) score: DeterministicScore,
}

/// Where an asset appeared in the visual and caption rankings before fusion.
#[derive(Clone, Copy, Default)]
pub(crate) struct FusedEvidence {
    pub(crate) visual: Option<ListPosition>,
    pub(crate) descriptor: Option<ListPosition>,
}

/// Materialize an already fused, ordered candidate list through the same
/// bounded ranked-prefix path as the visual mode, then attach the per-list
/// evidence to each accepted hit.
///
/// `score` is the fused score. `visual_score` is the cosine (validated like the
/// visual mode validates it) and `descriptor_score` is the caption ranking's
/// score; each of the four evidence keys is `null` when the asset was absent
/// from that list.
pub(crate) async fn materialize_fused_hits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    fused: Vec<(Uuid, DeterministicScore)>,
    evidence: &BTreeMap<Uuid, FusedEvidence>,
    top_k: u32,
) -> Result<Vec<Value>, RuntimeError> {
    let candidates = fused
        .into_iter()
        .map(|(key, score)| RankedCandidate { key, score })
        .collect();
    let materialized = materialize_ranked_candidates(
        runtime,
        token,
        blob_store,
        query_asset_id,
        candidates,
        top_k,
        validated_fused_score_value,
    )
    .await?;
    materialized
        .accepted
        .into_iter()
        .map(|item| -> Result<Value, RuntimeError> {
            let key = item.candidate.key;
            let found = evidence.get(&key).copied().unwrap_or_default();
            let visual_score = match found.visual {
                Some(position) => Value::from(validated_cosine_score_value(key, position.score)?),
                None => Value::Null,
            };
            let visual_rank = found
                .visual
                .map_or(Value::Null, |position| Value::from(position.rank));
            let descriptor_score = found
                .descriptor
                .map_or(Value::Null, |position| Value::from(position.score.to_f64()));
            let descriptor_rank = found
                .descriptor
                .map_or(Value::Null, |position| Value::from(position.rank));
            let mut output = item.output;
            if let Some(fields) = output.as_object_mut() {
                fields.insert("visual_score".to_string(), visual_score);
                fields.insert("visual_rank".to_string(), visual_rank);
                fields.insert("descriptor_score".to_string(), descriptor_score);
                fields.insert("descriptor_rank".to_string(), descriptor_rank);
            }
            Ok(output)
        })
        .collect()
}

async fn load_moodboard_candidate_batch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blob_store: &dyn BlobStore,
    query_asset_id: Uuid,
    authorized_namespaces: &BTreeSet<String>,
    keys: Vec<Uuid>,
) -> Result<Vec<(Uuid, MoodboardCandidateRow)>, RuntimeError> {
    let [subject_id] = keys.as_slice() else {
        return Err(RuntimeError::Internal(format!(
            "moodboard materialization expected a one-row loader batch, got {}",
            keys.len()
        )));
    };
    let subject_id = *subject_id;
    if subject_id == query_asset_id {
        return Ok(vec![(
            subject_id,
            MoodboardCandidateRow::Drop(MoodboardDropReason::SelfHit),
        )]);
    }

    let candidate = match runtime.get_entity(token, subject_id).await {
        Ok(candidate) => candidate,
        Err(error) if is_stale_candidate_error(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let row = if !authorized_namespaces.contains(candidate.namespace.as_str()) {
        MoodboardCandidateRow::Drop(MoodboardDropReason::OutsideVisibleScope)
    } else if candidate.kind != "artifact" {
        MoodboardCandidateRow::Drop(MoodboardDropReason::WrongKind)
    } else if candidate.entity_type.as_deref() != Some("visual_asset") {
        MoodboardCandidateRow::Drop(MoodboardDropReason::WrongSubtype)
    } else if let Some(candidate_ref) = candidate.content_ref.as_deref() {
        match ContentRef::from_hex(candidate_ref) {
            Ok(candidate_ref) => {
                if blob_store.exists(&candidate_ref).await? {
                    MoodboardCandidateRow::Keep {
                        asset_id: candidate.id,
                        name: candidate.name,
                        content_ref: candidate_ref,
                    }
                } else {
                    MoodboardCandidateRow::Drop(MoodboardDropReason::MissingBlob)
                }
            }
            Err(_) => MoodboardCandidateRow::Drop(MoodboardDropReason::MalformedContentRef),
        }
    } else {
        MoodboardCandidateRow::Drop(MoodboardDropReason::MissingContentAttachment)
    };
    Ok(vec![(subject_id, row)])
}

fn map_moodboard_materialization_error(error: MaterializationError<RuntimeError>) -> RuntimeError {
    match error {
        MaterializationError::Caller(error) => error,
        structural => RuntimeError::Internal(format!(
            "moodboard ranked materialization invariant failed: {structural}"
        )),
    }
}

pub(crate) fn is_stale_candidate_error(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::NotFound(_) | RuntimeError::NamespaceMismatch { .. }
    )
}

/// Preserve the existing per-subject maximum and canonical tie order.
pub(crate) fn merge_visible_hits(
    sources: Vec<Vec<VectorSearchHit>>,
    limit: u32,
) -> Vec<VectorSearchHit> {
    union_fusion(
        sources
            .into_iter()
            .map(|source| {
                source
                    .into_iter()
                    .map(|hit| (hit.subject_id, hit.score))
                    .collect()
            })
            .collect(),
    )
    .into_iter()
    .take(limit as usize)
    .enumerate()
    .map(|(index, (subject_id, score))| VectorSearchHit {
        subject_id,
        score,
        rank: u32::try_from(index + 1).expect("limit is bounded to u32"),
    })
    .collect()
}

#[cfg(test)]
#[path = "materialization_tests.rs"]
mod tests;
