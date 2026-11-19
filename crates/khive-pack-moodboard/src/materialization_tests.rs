use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use khive_runtime::Namespace;
use khive_storage::types::{SqlStatement, SqlValue, StorageResult};
use khive_storage::{Attachment, AttachmentSubstrate, Entity, StorageCapability, StorageError};

use super::*;

#[derive(Debug, Default)]
struct MetadataStore {
    objects: BTreeMap<ContentRef, bool>,
    failure: Option<ContentRef>,
    calls: Mutex<Vec<(String, ContentRef)>>,
}

impl MetadataStore {
    fn record(&self, operation: &str, content_ref: &ContentRef) {
        self.calls
            .lock()
            .unwrap()
            .push((operation.into(), content_ref.clone()));
    }

    fn operations(&self) -> Vec<(String, ContentRef)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl BlobStore for MetadataStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        panic!("candidate materialization must not write blobs")
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        _max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.record("hydrate", content_ref);
        Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: "candidate_hydration".into(),
            message: "metadata fixture has no bytes".into(),
        })
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.record("exists", content_ref);
        if self.failure.as_ref() == Some(content_ref) {
            return Err(StorageError::driver(
                StorageCapability::Blob,
                "exists_fixture",
                std::io::Error::other("metadata unavailable"),
            ));
        }
        Ok(self.objects.get(content_ref).copied().unwrap_or(false))
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        self.record("size", content_ref);
        Ok(None)
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        panic!("candidate materialization must not delete blobs")
    }
}

fn hit(id: u128, score: f64) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: Uuid::from_u128(id),
        score: DeterministicScore::from_f64(score),
        rank: 42,
    }
}

fn digest(digit: char) -> ContentRef {
    ContentRef::from_hex(digit.to_string().repeat(64)).unwrap()
}

#[tokio::test]
async fn deleted_and_missing_candidates_drop_without_blob_io() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut deleted = asset(
        &runtime,
        &token,
        1,
        token.namespace().as_str(),
        "artifact",
        Some("visual_asset"),
        Some(("content", digest('a'))),
    )
    .await;
    deleted.deleted_at = Some(1);
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(deleted)
        .await
        .unwrap();
    let store = MetadataStore::default();
    let result = materialize_hits_with_diagnostics(
        &runtime,
        &token,
        &store,
        Uuid::nil(),
        vec![hit(1, 0.9), hit(2, 0.8)],
        2,
    )
    .await
    .unwrap();
    assert!(result.accepted.is_empty());
    assert_eq!(
        result.drop_counts.count(MoodboardDropReason::StaleEntity),
        Some(2)
    );
    assert_eq!(
        result
            .diagnostic_details
            .iter()
            .map(|detail| detail.candidate.key)
            .collect::<Vec<_>>(),
        [Uuid::from_u128(1), Uuid::from_u128(2)]
    );
    assert!(store.operations().is_empty());
}

#[tokio::test]
async fn invalid_self_score_fails_before_self_policy_or_entity_io() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    asset(
        &runtime,
        &token,
        1,
        token.namespace().as_str(),
        "artifact",
        Some("visual_asset"),
        None,
    )
    .await;
    corrupt_tags(&runtime, Uuid::from_u128(1), false).await;
    let store = MetadataStore::default();
    let error = materialize_hits(
        &runtime,
        &token,
        &store,
        Uuid::from_u128(1),
        vec![hit(1, -2.0)],
        1,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        validated_cosine_score_value(Uuid::from_u128(1), hit(1, -2.0).score)
            .unwrap_err()
            .to_string()
    );
    assert!(store.operations().is_empty());
}

#[allow(clippy::too_many_arguments)]
async fn asset(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: u128,
    namespace: &str,
    kind: &str,
    subtype: Option<&str>,
    attachment: Option<(&str, ContentRef)>,
) -> Entity {
    let mut entity =
        Entity::new(namespace, kind, format!("candidate {id}")).with_entity_type(subtype);
    entity.id = Uuid::from_u128(id);
    runtime
        .entities(token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    if let Some((role, content_ref)) = attachment {
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: entity.id,
                substrate: AttachmentSubstrate::Entity,
                role: role.into(),
                content_ref,
                media_type: None,
                size_bytes: None,
                created_at: entity.created_at,
            })
            .await
            .unwrap();
    }
    entity
}

async fn corrupt_tags(runtime: &KhiveRuntime, id: Uuid, deleted: bool) {
    let mut writer = runtime.sql().writer().await.unwrap();
    let changed = writer
        .execute(SqlStatement {
            sql: "UPDATE entities SET tags = 'not json', deleted_at = ?1, version = version + 1 WHERE id = ?2".into(),
            params: vec![
                if deleted {
                    SqlValue::Integer(1)
                } else {
                    SqlValue::Null
                },
                SqlValue::Text(id.to_string()),
            ],
            label: Some("moodboard_corrupt_tags_fixture".into()),
        })
        .await
        .unwrap();
    assert_eq!(changed, 1, "corruption fixture must modify its own row");
}

async fn corrupt_content_ref(runtime: &KhiveRuntime, id: Uuid) {
    let mut writer = runtime.sql().writer().await.unwrap();
    writer
        .execute_script("PRAGMA ignore_check_constraints = ON;".into())
        .await
        .unwrap();
    let changed = writer.execute(SqlStatement {
        sql: "UPDATE attachments SET content_ref = 'malformed' WHERE record_uuid = ?1 AND role = 'content'".into(),
        params: vec![SqlValue::Text(id.to_string())],
        label: Some("moodboard_corrupt_content_ref_fixture".into()),
    }).await.unwrap();
    writer
        .execute_script("PRAGMA ignore_check_constraints = OFF;".into())
        .await
        .unwrap();
    assert_eq!(changed, 1, "malformed reference fixture must be populated");
}

#[tokio::test]
async fn typed_policy_drops_compact_without_hydrating_candidates() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let ns = token.namespace().as_str();
    let kept_ref = digest('a');
    let missing_ref = digest('b');
    asset(
        &runtime,
        &token,
        3,
        "foreign",
        "artifact",
        Some("visual_asset"),
        Some(("content", kept_ref.clone())),
    )
    .await;
    asset(
        &runtime,
        &token,
        4,
        ns,
        "concept",
        Some("visual_asset"),
        Some(("content", kept_ref.clone())),
    )
    .await;
    asset(
        &runtime,
        &token,
        5,
        ns,
        "artifact",
        Some("other"),
        Some(("content", kept_ref.clone())),
    )
    .await;
    asset(
        &runtime,
        &token,
        6,
        ns,
        "artifact",
        Some("visual_asset"),
        Some(("thumbnail", kept_ref.clone())),
    )
    .await;
    asset(
        &runtime,
        &token,
        7,
        ns,
        "artifact",
        Some("visual_asset"),
        Some(("content", kept_ref.clone())),
    )
    .await;
    corrupt_content_ref(&runtime, Uuid::from_u128(7)).await;
    asset(
        &runtime,
        &token,
        8,
        ns,
        "artifact",
        Some("visual_asset"),
        Some(("content", missing_ref.clone())),
    )
    .await;
    asset(
        &runtime,
        &token,
        9,
        ns,
        "artifact",
        Some("visual_asset"),
        Some(("content", kept_ref.clone())),
    )
    .await;
    // A projected attachment string reaches pack policy; hydration is unnecessary.
    assert_eq!(
        runtime
            .get_entity(&token, Uuid::from_u128(7))
            .await
            .unwrap()
            .content_ref
            .as_deref(),
        Some("malformed")
    );
    let store = MetadataStore {
        objects: BTreeMap::from([(kept_ref.clone(), true)]),
        ..Default::default()
    };
    let scores = [1.0, 0.95, 0.9, 0.85, 0.8, 0.75, 0.7, 0.65, 0.6];
    let result = materialize_hits_with_diagnostics(
        &runtime,
        &token,
        &store,
        Uuid::from_u128(1),
        scores
            .into_iter()
            .enumerate()
            .map(|(i, score)| hit((i + 1) as u128, score))
            .collect(),
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        result.accepted.len(),
        1,
        "stale and ineligible rows honestly underfill"
    );
    assert_eq!(result.accepted[0].candidate.key, Uuid::from_u128(9));
    assert_eq!(result.accepted[0].candidate.score, hit(9, 0.6).score);
    assert_eq!(result.accepted[0].rank, 1);
    assert_eq!(
        result.accepted[0].output,
        json!({"asset_id": Uuid::from_u128(9).to_string(), "score": hit(9, 0.6).score.to_f64(), "rank": 1, "name": "candidate 9", "content_ref": kept_ref.to_string()})
    );
    assert_eq!(result.drop_counts.total(), 8);
    for reason in MoodboardDropReason::ALL {
        assert_eq!(result.drop_counts.count(*reason), Some(1), "{reason:?}");
    }
    assert_eq!(
        result
            .diagnostic_details
            .iter()
            .map(|detail| (detail.candidate.key, detail.reason))
            .collect::<Vec<_>>(),
        MoodboardDropReason::ALL
            .iter()
            .enumerate()
            .map(|(i, reason)| (Uuid::from_u128((i + 1) as u128), *reason))
            .collect::<Vec<_>>()
    );
    assert!(!result.diagnostics_truncated);
    assert_eq!(
        store.operations(),
        vec![("exists".into(), missing_ref), ("exists".into(), kept_ref)]
    );
}

#[tokio::test]
async fn self_hit_and_post_k_rows_never_read_their_poisoned_entities() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let content_ref = digest('a');
    for id in [1, 2, 3] {
        asset(
            &runtime,
            &token,
            id,
            token.namespace().as_str(),
            "artifact",
            Some("visual_asset"),
            Some(("content", content_ref.clone())),
        )
        .await;
    }
    corrupt_tags(&runtime, Uuid::from_u128(1), false).await;
    corrupt_tags(&runtime, Uuid::from_u128(3), false).await;
    let store = MetadataStore {
        objects: BTreeMap::from([(content_ref.clone(), true)]),
        ..Default::default()
    };
    let result = materialize_hits_with_diagnostics(
        &runtime,
        &token,
        &store,
        Uuid::from_u128(1),
        vec![hit(1, 1.0), hit(2, 0.9), hit(3, 0.8)],
        1,
    )
    .await;
    assert!(
        result.is_ok(),
        "self and post-K entities must not be read: {result:?}"
    );
    let result = result.unwrap();
    assert_eq!(result.accepted[0].candidate.key, Uuid::from_u128(2));
    assert_eq!(
        result.drop_counts.count(MoodboardDropReason::SelfHit),
        Some(1)
    );
    assert_eq!(store.operations(), vec![("exists".into(), content_ref)]);
}

#[tokio::test]
async fn invalid_tail_score_is_fatal_after_k_without_tail_io() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let content_ref = digest('a');
    asset(
        &runtime,
        &token,
        1,
        token.namespace().as_str(),
        "artifact",
        Some("visual_asset"),
        Some(("content", content_ref.clone())),
    )
    .await;
    let store = MetadataStore {
        objects: BTreeMap::from([(content_ref.clone(), true)]),
        ..Default::default()
    };
    let error = materialize_hits(
        &runtime,
        &token,
        &store,
        Uuid::nil(),
        vec![hit(1, 0.9), hit(2, -2.0)],
        1,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        validated_cosine_score_value(Uuid::from_u128(2), hit(2, -2.0).score)
            .unwrap_err()
            .to_string()
    );
    assert_eq!(store.operations(), vec![("exists".into(), content_ref)]);
}

#[tokio::test]
async fn earlier_metadata_failure_wins_over_later_invalid_score() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let content_ref = digest('a');
    asset(
        &runtime,
        &token,
        1,
        token.namespace().as_str(),
        "artifact",
        Some("visual_asset"),
        Some(("content", content_ref.clone())),
    )
    .await;
    let store = MetadataStore {
        failure: Some(content_ref.clone()),
        ..Default::default()
    };
    let error = materialize_hits(
        &runtime,
        &token,
        &store,
        Uuid::nil(),
        vec![hit(1, 0.9), hit(2, -2.0)],
        2,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Storage(StorageError::Driver { operation, .. }) if operation == "exists_fixture")
    );
    assert_eq!(store.operations(), vec![("exists".into(), content_ref)]);
}

#[tokio::test]
async fn corrupt_tombstone_probe_is_fatal_before_later_score_validation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    asset(
        &runtime,
        &token,
        1,
        token.namespace().as_str(),
        "artifact",
        Some("visual_asset"),
        None,
    )
    .await;
    corrupt_tags(&runtime, Uuid::from_u128(1), true).await;
    let store = MetadataStore::default();
    let error = materialize_hits(
        &runtime,
        &token,
        &store,
        Uuid::nil(),
        vec![hit(1, 0.9), hit(2, -2.0)],
        2,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Storage(StorageError::Driver { operation, .. }) if operation == "get_entity_including_deleted")
    );
    assert!(store.operations().is_empty());
}

#[tokio::test]
async fn foreign_entity_integrity_is_checked_before_scope_eligibility() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    asset(
        &runtime,
        &token,
        1,
        "foreign",
        "artifact",
        Some("visual_asset"),
        None,
    )
    .await;
    corrupt_tags(&runtime, Uuid::from_u128(1), false).await;
    let store = MetadataStore::default();
    let error = materialize_hits(
        &runtime,
        &token,
        &store,
        Uuid::nil(),
        vec![hit(1, 0.9), hit(2, -2.0)],
        2,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Storage(StorageError::Driver { operation, .. }) if operation == "get_entity")
    );
    assert!(store.operations().is_empty());
}

#[test]
fn union_merge_retains_max_scores_and_canonical_uuid_ties() {
    let inputs = vec![
        vec![hit(3, 0.9), hit(1, 0.8), hit(2, 0.7)],
        vec![hit(2, 0.9), hit(1, 0.9), hit(3, 0.6)],
    ];
    let merged = merge_visible_hits(inputs.clone(), 2);
    assert_eq!(
        merged
            .iter()
            .map(|hit| (hit.subject_id, hit.score, hit.rank))
            .collect::<Vec<_>>(),
        vec![
            (Uuid::from_u128(1), hit(1, 0.9).score, 1),
            (Uuid::from_u128(2), hit(2, 0.9).score, 2)
        ]
    );
    let reordered = merge_visible_hits(
        inputs
            .into_iter()
            .rev()
            .map(|source| source.into_iter().rev().collect())
            .collect(),
        2,
    );
    assert_eq!(
        reordered
            .iter()
            .map(|hit| (hit.subject_id, hit.score, hit.rank))
            .collect::<Vec<_>>(),
        merged
            .iter()
            .map(|hit| (hit.subject_id, hit.score, hit.rank))
            .collect::<Vec<_>>()
    );
}
