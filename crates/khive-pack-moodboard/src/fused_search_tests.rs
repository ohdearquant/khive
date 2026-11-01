// Tests for `moodboard.search` fused reranking. The vision model is not
// involved: each test supplies the query embedding directly and builds a small
// corpus in an in-memory runtime. Caption words are chosen so that only the
// intended assets share words with the query asset's text ("harbor lantern").

use khive_runtime::Namespace;
use khive_storage::{Attachment, AttachmentSubstrate};

use super::*;

const QUERY: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const TOLERANCE: f64 = 1e-8;

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    descriptor: DescriptorIdentity,
    blob_store: khive_db::stores::blob::FsBlobStore,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let blob_store =
            khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0).unwrap();
        Self {
            runtime,
            token,
            descriptor: DescriptorIdentity::fixture(4),
            blob_store,
            _root: root,
        }
    }

    /// A live visual asset with a stored blob, a content attachment and a
    /// text-index entry. When `cosine` is present a visual vector with that
    /// cosine to `QUERY` is indexed as well.
    async fn add_asset(
        &self,
        id: u128,
        name: &str,
        caption: Option<&str>,
        cosine: Option<f32>,
    ) -> Entity {
        let content_ref = self
            .blob_store
            .put(format!("fused search fixture {id}").into_bytes())
            .await
            .unwrap();
        let mut entity = Entity::new(self.token.namespace().as_str(), "artifact", name)
            .with_entity_type(Some("visual_asset"));
        if let Some(caption) = caption {
            entity = entity.with_description(caption);
        }
        entity.id = Uuid::from_u128(id);
        self.runtime
            .entities(&self.token)
            .unwrap()
            .upsert_entity(entity.clone())
            .await
            .unwrap();
        self.runtime
            .reindex_claimed_entity(&self.token, &entity)
            .await
            .unwrap();
        self.runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment {
                record_uuid: entity.id,
                substrate: AttachmentSubstrate::Entity,
                role: "content".into(),
                content_ref,
                media_type: None,
                size_bytes: None,
                created_at: entity.created_at,
            })
            .await
            .unwrap();
        if let Some(cosine) = cosine {
            self.index_cosine(id, cosine).await;
        }
        entity
    }

    /// Index a unit vector whose cosine with `QUERY` is `cosine`.
    async fn index_cosine(&self, id: u128, cosine: f32) {
        let embedding = [cosine, (1.0 - cosine * cosine).sqrt(), 0.0, 0.0];
        index_embedding(
            &self.runtime,
            &self.token,
            &self.descriptor,
            Uuid::from_u128(id),
            &embedding,
        )
        .await
        .unwrap();
    }

    /// The query asset: name "harbor", caption "lantern", vector equal to `QUERY`.
    async fn seed_query(&self) -> Entity {
        self.add_asset(1, "harbor", Some("lantern"), Some(1.0))
            .await
    }

    /// Query asset 1 plus candidates 2, 3, 4 whose visual order is 2, 3, 4.
    /// Only candidate 4 has a caption matching the query asset's text.
    async fn seed_ranked(&self) -> Entity {
        let query = self.seed_query().await;
        self.add_asset(2, "granite", Some("meadow"), Some(0.9))
            .await;
        self.add_asset(3, "violet", Some("orchard"), Some(0.6))
            .await;
        self.add_asset(4, "dockside", Some("harbor lantern glow"), Some(0.3))
            .await;
        query
    }
}

fn uuid_of(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn uid(n: u128) -> String {
    uuid_of(n).to_string()
}

fn hit_ids(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .map(|hit| hit["asset_id"].as_str().unwrap().to_string())
        .collect()
}

fn hit_for(hits: &[Value], id: u128) -> &Value {
    let wanted = uid(id);
    hits.iter()
        .find(|hit| hit["asset_id"].as_str() == Some(wanted.as_str()))
        .unwrap_or_else(|| panic!("asset {wanted} is not among the hits"))
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap()
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < TOLERANCE,
        "expected {expected}, got {actual}"
    );
}

fn assert_null_field(hit: &Value, key: &str) {
    assert_eq!(
        hit.get(key),
        Some(&Value::Null),
        "{key} must be present and null"
    );
}

/// Weighted reciprocal-rank term for a 1-based rank.
fn weighted_rrf(weight: f64, k: usize, rank: usize) -> f64 {
    weight / (k + rank) as f64
}

fn score_of(list: &[(Uuid, f64)], id: u128) -> f64 {
    list.iter()
        .find(|(found, _)| *found == uuid_of(id))
        .map(|(_, score)| *score)
        .unwrap_or_else(|| panic!("asset {} is not in the list", uid(id)))
}

/// The visual list exactly as `search_embedding` returns it, query asset included.
async fn direct_visual(f: &Fixture, top_k: u32) -> Vec<(Uuid, f64)> {
    search_embedding(
        &f.runtime,
        &f.token,
        &f.descriptor,
        &QUERY,
        candidate_limit(top_k),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|hit| (hit.subject_id, hit.score.to_f64()))
    .collect()
}

/// The caption list exactly as `hybrid_search` returns it, query asset included.
async fn direct_descriptor(f: &Fixture, text: &str, top_k: u32) -> Vec<(Uuid, f64)> {
    f.runtime
        .hybrid_search(
            &f.token,
            text,
            None,
            candidate_limit(top_k),
            Some("artifact"),
            Some("visual_asset"),
            &[],
            None,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|hit| (hit.entity_id, hit.score.to_f64()))
        .collect()
}

async fn run_fused(
    f: &Fixture,
    query: &Entity,
    top_k: u32,
    descriptor_weight: f64,
    rrf_k: usize,
) -> FusedSearch {
    search_fused_hits(
        &f.runtime,
        &f.runtime,
        &f.token,
        &f.blob_store,
        query,
        &f.descriptor,
        &QUERY,
        top_k,
        descriptor_weight,
        rrf_k,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn visual_mode_matches_the_exact_cosine_path_and_adds_no_fused_keys() {
    let f = Fixture::new();
    let query = f.seed_ranked().await;
    let expected = search_materialized_hits(
        &f.runtime,
        &f.runtime,
        &f.token,
        &f.blob_store,
        query.id,
        &f.descriptor,
        &QUERY,
        5,
    )
    .await
    .unwrap();
    assert_eq!(hit_ids(&expected), vec![uid(2), uid(3), uid(4)]);

    let asset_id = query.id.to_string();
    let requests = [
        json!({ "asset_id": asset_id }),
        json!({ "asset_id": asset_id, "rerank": "visual" }),
        json!({ "asset_id": asset_id, "rerank": null, "descriptor_weight": null, "rrf_k": null }),
    ];
    for params in requests {
        let mode = parse_rerank(&params).unwrap();
        assert_eq!(mode, RerankMode::Visual);
        let (hits, report) = search_by_mode(
            &f.runtime,
            &f.runtime,
            &f.token,
            &f.blob_store,
            &query,
            &f.descriptor,
            &QUERY,
            5,
            mode,
        )
        .await
        .unwrap();
        assert!(report.is_none());
        assert_eq!(hits, expected);
        for hit in &hits {
            assert_eq!(
                sorted_keys(hit),
                ["asset_id", "content_ref", "name", "rank", "score"]
            );
        }
        let response = search_response(query.id, &f.descriptor, hits, report);
        assert_eq!(
            sorted_keys(&response),
            ["descriptor", "experimental", "hits", "query_asset_id"]
        );
        assert_eq!(response["experimental"], json!(true));
    }
}

#[tokio::test]
async fn fused_order_follows_the_weighted_reciprocal_rank_formula() {
    let f = Fixture::new();
    let query = f.seed_ranked().await;
    let visual = direct_visual(&f, 5).await;
    let direct_text = direct_descriptor(&f, "harbor lantern", 5).await;
    let visual_ids: Vec<Uuid> = visual.iter().map(|(found, _)| *found).collect();
    assert_eq!(
        visual_ids,
        vec![uuid_of(1), uuid_of(2), uuid_of(3), uuid_of(4)]
    );
    let mut text_ids: Vec<Uuid> = direct_text.iter().map(|(found, _)| *found).collect();
    text_ids.sort();
    assert_eq!(text_ids, vec![uuid_of(1), uuid_of(4)]);

    let (hits, report) = search_by_mode(
        &f.runtime,
        &f.runtime,
        &f.token,
        &f.blob_store,
        &query,
        &f.descriptor,
        &QUERY,
        5,
        RerankMode::Fused {
            descriptor_weight: 0.5,
            rrf_k: 60,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        report.unwrap(),
        json!({
            "mode": "fused",
            "descriptor_weight": 0.5,
            "rrf_k": 60,
            "descriptor_arm": "ran",
            "visual_candidates": 3,
            "descriptor_candidates": 1,
        })
    );
    // Asset 4 is third by cosine but first by caption, so it overtakes 2 and 3.
    assert_eq!(hit_ids(&hits), vec![uid(4), uid(2), uid(3)]);
    for hit in &hits {
        assert_eq!(
            sorted_keys(hit),
            [
                "asset_id",
                "content_ref",
                "descriptor_rank",
                "descriptor_score",
                "name",
                "rank",
                "score",
                "visual_rank",
                "visual_score"
            ]
        );
    }

    let (w, k) = (0.5, 60);
    let fourth = hit_for(&hits, 4);
    assert_close(
        number(&fourth["score"]),
        weighted_rrf(w, k, 1) + weighted_rrf(w, k, 3),
    );
    assert_eq!(fourth["visual_rank"], json!(3));
    assert_eq!(fourth["descriptor_rank"], json!(1));
    assert_close(number(&fourth["visual_score"]), score_of(&visual, 4));
    assert_close(
        number(&fourth["descriptor_score"]),
        score_of(&direct_text, 4),
    );

    let second = hit_for(&hits, 2);
    assert_close(number(&second["score"]), weighted_rrf(w, k, 1));
    assert_eq!(second["visual_rank"], json!(1));
    assert_close(number(&second["visual_score"]), score_of(&visual, 2));
    assert_null_field(second, "descriptor_score");
    assert_null_field(second, "descriptor_rank");

    let third = hit_for(&hits, 3);
    assert_close(number(&third["score"]), weighted_rrf(w, k, 2));
    assert_eq!(third["visual_rank"], json!(2));
    assert_close(number(&third["visual_score"]), score_of(&visual, 3));
    assert_null_field(third, "descriptor_score");
    assert_null_field(third, "descriptor_rank");
}

#[tokio::test]
async fn caption_only_candidate_is_returned_without_visual_evidence() {
    let f = Fixture::new();
    let query = f.seed_query().await;
    f.add_asset(2, "granite", Some("meadow"), Some(0.9)).await;
    f.add_asset(3, "violet", Some("orchard"), Some(0.6)).await;
    // Asset 5 has no visual vector; only its caption matches the query text.
    f.add_asset(5, "pier", Some("harbor lantern"), None).await;
    let visual = direct_visual(&f, 5).await;
    assert!(visual.iter().all(|(found, _)| *found != uuid_of(5)));
    let direct_text = direct_descriptor(&f, "harbor lantern", 5).await;

    let descriptor_weight = 0.6;
    let visual_weight = 1.0 - descriptor_weight;
    let fused = run_fused(&f, &query, 5, descriptor_weight, 60).await;
    assert!(fused.descriptor_arm_ran);
    assert_eq!(fused.visual_candidates, 2);
    assert_eq!(fused.descriptor_candidates, 1);
    assert_eq!(hit_ids(&fused.hits), vec![uid(5), uid(2), uid(3)]);

    let pier = hit_for(&fused.hits, 5);
    assert_close(
        number(&pier["score"]),
        weighted_rrf(descriptor_weight, 60, 1),
    );
    assert_null_field(pier, "visual_score");
    assert_null_field(pier, "visual_rank");
    assert_eq!(pier["descriptor_rank"], json!(1));
    assert_close(number(&pier["descriptor_score"]), score_of(&direct_text, 5));

    let granite = hit_for(&fused.hits, 2);
    assert_close(
        number(&granite["score"]),
        weighted_rrf(visual_weight, 60, 1),
    );
    assert_eq!(granite["visual_rank"], json!(1));
    let violet = hit_for(&fused.hits, 3);
    assert_close(number(&violet["score"]), weighted_rrf(visual_weight, 60, 2));
    assert_eq!(violet["visual_rank"], json!(2));
}

#[tokio::test]
async fn query_asset_without_a_caption_skips_the_caption_ranking() {
    let f = Fixture::new();
    // The generated default name must never serve as caption text, so a
    // candidate whose caption repeats it must not be found through it.
    let base = f.add_asset(1, "asset-0001", None, Some(1.0)).await;
    f.add_asset(2, "granite", Some("meadow"), Some(0.9)).await;
    f.add_asset(3, "violet", Some("orchard"), Some(0.6)).await;
    f.add_asset(4, "dockside", Some("asset-0001 mirror"), Some(0.3))
        .await;
    let visual_path = search_materialized_hits(
        &f.runtime,
        &f.runtime,
        &f.token,
        &f.blob_store,
        base.id,
        &f.descriptor,
        &QUERY,
        5,
    )
    .await
    .unwrap();

    for description in [None, Some(""), Some("   ")] {
        let mut query = base.clone();
        query.description = description.map(str::to_string);
        let fused = run_fused(&f, &query, 5, 0.5, 60).await;
        assert!(!fused.descriptor_arm_ran, "description {description:?}");
        assert_eq!(fused.descriptor_candidates, 0);
        assert_eq!(fused.visual_candidates, 3);
        assert_eq!(
            fused_report(0.5, 60, &fused)["descriptor_arm"],
            json!("skipped_no_caption")
        );
        assert_eq!(hit_ids(&fused.hits), hit_ids(&visual_path));
        assert_eq!(hit_ids(&fused.hits), vec![uid(2), uid(3), uid(4)]);
        for (index, hit) in fused.hits.iter().enumerate() {
            assert_close(number(&hit["score"]), weighted_rrf(0.5, 60, index + 1));
            assert_eq!(hit["visual_rank"], json!(index + 1));
            assert_null_field(hit, "descriptor_score");
            assert_null_field(hit, "descriptor_rank");
        }
    }
}

#[tokio::test]
async fn query_asset_is_excluded_from_both_lists_and_the_hits() {
    let f = Fixture::new();
    let query = f.seed_ranked().await;
    let visual = direct_visual(&f, 5).await;
    let direct_text = direct_descriptor(&f, "harbor lantern", 5).await;
    assert!(visual.iter().any(|(found, _)| *found == query.id));
    assert!(direct_text.iter().any(|(found, _)| *found == query.id));

    let fused = run_fused(&f, &query, 5, 0.5, 60).await;
    assert_eq!(fused.visual_candidates, visual.len() - 1);
    assert_eq!(fused.descriptor_candidates, direct_text.len() - 1);
    assert!(!hit_ids(&fused.hits).contains(&uid(1)));
    // Ranks start at the first asset other than the query asset.
    assert_eq!(hit_for(&fused.hits, 2)["visual_rank"], json!(1));
    assert_eq!(hit_for(&fused.hits, 4)["descriptor_rank"], json!(1));
}

#[tokio::test]
async fn unmaterializable_candidates_are_dropped_but_keep_their_list_rank() {
    let f = Fixture::new();
    let query = f.seed_query().await;
    f.add_asset(2, "granite", Some("meadow"), Some(0.9)).await;
    // A vector with no live entity ranks above asset 2 and must be dropped.
    f.index_cosine(9, 0.95).await;

    let fused = run_fused(&f, &query, 5, 0.5, 60).await;
    assert!(fused.descriptor_arm_ran);
    assert_eq!(fused.visual_candidates, 2);
    assert_eq!(fused.descriptor_candidates, 0);
    assert_eq!(hit_ids(&fused.hits), vec![uid(2)]);
    let granite = hit_for(&fused.hits, 2);
    assert_eq!(granite["visual_rank"], json!(2));
    assert_close(number(&granite["score"]), weighted_rrf(0.5, 60, 2));
}
