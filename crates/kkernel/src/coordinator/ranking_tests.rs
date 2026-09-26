use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use khive_mcp::coordinator::CoordinatorService;
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, NoteSearchHit, PackRegistry, RankScoreKind, RuntimeConfig,
    SearchHit, SearchSignals, SearchSource, StorageBackend, VerbRegistryBuilder,
};
use khive_score::DeterministicScore;
use khive_storage::{Entity, Note};
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::dispatch::{rrf_merge_entity_hits, rrf_merge_note_hits};
use super::{BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService};

fn memory_runtime() -> Arc<KhiveRuntime> {
    let backend = Arc::new(StorageBackend::memory().expect("in-memory backend"));
    backend.prepare_core_schema().expect("core schema");
    Arc::new(KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:coordinator-ranking".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    ))
}

fn request(params: serde_json::Value) -> ValidatedSearchRequest {
    let runtime = memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(runtime.config().gate.clone());
    builder.with_default_namespace(Namespace::LOCAL);
    builder.with_actor_id(runtime.config().actor_id.clone());
    PackRegistry::register_packs(&["kg".into()], (*runtime).clone(), &mut builder)
        .expect("KG pack registration");
    ValidatedSearchRequest::from_value(params, &builder.build().expect("registry"))
        .expect("valid search request")
}

fn backend_id(id: &str) -> BackendId {
    BackendId::parse(id).expect("backend id")
}

fn keyword(raw: i64) -> SearchSignals {
    SearchSignals {
        vector_similarity: None,
        keyword_score: Some(DeterministicScore::from_raw(raw)),
    }
}

fn vector(raw: i64) -> SearchSignals {
    SearchSignals {
        vector_similarity: Some(DeterministicScore::from_raw(raw)),
        keyword_score: None,
    }
}

fn entity_hit(id: Uuid, kind: RankScoreKind, signals: SearchSignals) -> SearchHit {
    SearchHit {
        entity_id: id,
        score: DeterministicScore::from_raw(987_654_321),
        rank_score_kind: kind,
        signals,
        source: match (signals.vector_similarity, signals.keyword_score) {
            (Some(_), Some(_)) => SearchSource::Both,
            (Some(_), None) => SearchSource::Vector,
            _ => SearchSource::Text,
        },
        title: Some("candidate".into()),
        snippet: Some("retained snippet".into()),
    }
}

fn as_note(hit: SearchHit) -> NoteSearchHit {
    NoteSearchHit {
        note_id: hit.entity_id,
        score: hit.score,
        rank_score_kind: hit.rank_score_kind,
        signals: hit.signals,
        source: hit.source,
        title: hit.title,
        snippet: hit.snippet,
    }
}

fn assert_entity_eq(actual: &SearchHit, expected: &SearchHit) {
    assert_eq!(actual.entity_id, expected.entity_id);
    assert_eq!(actual.score, expected.score);
    assert_eq!(actual.rank_score_kind, expected.rank_score_kind);
    assert_eq!(actual.signals, expected.signals);
    assert_eq!(actual.source, expected.source);
    assert_eq!(actual.title, expected.title);
    assert_eq!(actual.snippet, expected.snippet);
}

fn assert_note_eq(actual: &NoteSearchHit, expected: &NoteSearchHit) {
    assert_eq!(actual.note_id, expected.note_id);
    assert_eq!(actual.score, expected.score);
    assert_eq!(actual.rank_score_kind, expected.rank_score_kind);
    assert_eq!(actual.signals, expected.signals);
    assert_eq!(actual.source, expected.source);
    assert_eq!(actual.title, expected.title);
    assert_eq!(actual.snippet, expected.snippet);
}

#[test]
fn outer_rrf_accumulates_quantized_contributions_for_entities_and_notes() {
    let id = Uuid::from_u128(1);
    let hit = entity_hit(id, RankScoreKind::Keyword, keyword(0));
    let entities = rrf_merge_entity_hits(vec![vec![hit.clone()]; 8], 10);
    let notes = rrf_merge_note_hits(vec![vec![as_note(hit)]; 8], 10);
    assert_eq!(entities.len(), 1);
    assert_eq!(notes.len(), 1);
    assert_eq!(entities[0].score.to_raw(), 563_274_400);
    assert_eq!(notes[0].score.to_raw(), 563_274_400);
    assert_eq!(entities[0].signals, keyword(0));
    assert_eq!(notes[0].signals, keyword(0));
}

#[test]
fn outer_rrf_balanced_permutations_keep_adjacent_ties() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    for orders in [[[a, b], [b, a]], [[b, a], [a, b]]] {
        for _ in 0..4 {
            let lists: Vec<Vec<_>> = orders
                .into_iter()
                .map(|ids| {
                    ids.into_iter()
                        .map(|id| entity_hit(id, RankScoreKind::Keyword, keyword(17)))
                        .collect()
                })
                .collect();
            let note_lists = lists
                .iter()
                .map(|list| list.iter().cloned().map(as_note).collect())
                .collect();
            let entities = rrf_merge_entity_hits(lists, 10);
            let notes = rrf_merge_note_hits(note_lists, 10);
            assert_eq!(
                entities.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
                vec![a, b]
            );
            assert_eq!(
                notes.iter().map(|hit| hit.note_id).collect::<Vec<_>>(),
                vec![a, b]
            );
            for hit in entities {
                assert_eq!(hit.score.to_raw(), 139_682_966);
                assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(hit.signals, keyword(17));
            }
            for hit in notes {
                assert_eq!(hit.score.to_raw(), 139_682_966);
                assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(hit.signals, keyword(17));
            }
        }
    }
}

#[test]
fn repeated_ids_select_one_best_ranked_signal_set() {
    let shared = Uuid::from_u128(1);
    let filler = Uuid::from_u128(2);
    let mut earlier = entity_hit(shared, RankScoreKind::Keyword, keyword(4_000_000_000));
    earlier.title = Some("earlier backend title".into());
    let later = entity_hit(shared, RankScoreKind::Vector, vector(0));
    let lists = vec![
        vec![
            entity_hit(filler, RankScoreKind::Union, keyword(1)),
            earlier,
        ],
        vec![later],
    ];
    let note_lists = lists
        .iter()
        .map(|list| list.iter().cloned().map(as_note).collect())
        .collect();
    let entities = rrf_merge_entity_hits(lists, 10);
    let notes = rrf_merge_note_hits(note_lists, 10);
    let entity = entities.iter().find(|hit| hit.entity_id == shared).unwrap();
    let note = notes.iter().find(|hit| hit.note_id == shared).unwrap();
    assert_eq!(entity.score.to_raw(), 139_682_966);
    assert_eq!(note.score.to_raw(), 139_682_966);
    assert_eq!(entity.rank_score_kind, RankScoreKind::Rrf);
    assert_eq!(note.rank_score_kind, RankScoreKind::Rrf);
    assert_eq!(entity.signals, vector(0));
    assert_eq!(note.signals, vector(0));
    assert_eq!(entity.source, SearchSource::Both);
    assert_eq!(note.source, SearchSource::Both);
    assert_eq!(entity.title.as_deref(), Some("earlier backend title"));
    assert_eq!(note.title.as_deref(), Some("earlier backend title"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn backend_registration_order_does_not_change_tied_evidence() {
    let shared = Uuid::from_u128(1);
    for order in [["alpha", "beta"], ["beta", "alpha"]] {
        let mut registry = BackendRegistry::new();
        for id in order {
            registry.register(backend_id(id), memory_runtime());
        }
        let overrides: HashMap<String, Vec<SearchHit>> = HashMap::from([
            (
                "alpha".into(),
                vec![entity_hit(shared, RankScoreKind::Keyword, keyword(17))],
            ),
            (
                "beta".into(),
                vec![entity_hit(shared, RankScoreKind::Vector, vector(900))],
            ),
        ]);
        let note_overrides: HashMap<String, Vec<NoteSearchHit>> = overrides
            .iter()
            .map(|(id, hits)| (id.clone(), hits.iter().cloned().map(as_note).collect()))
            .collect();
        let coordinator = SubstrateCoordinator::new(registry)
            .with_entity_hits_override(overrides)
            .with_note_hits_override(note_overrides);
        for kind in ["entity", "note"] {
            let request = request(serde_json::json!({"kind":kind,"query":"candidate","limit":10}));
            let (entities, notes, per_backend) = coordinator
                .fan_out_search(&request, &Namespace::local())
                .await;
            assert_eq!(
                per_backend
                    .iter()
                    .map(|row| row.backend_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["alpha", "beta"]
            );
            assert!(per_backend.iter().all(|row| row.error.is_none()));
            if kind == "entity" {
                assert_eq!(entities.len(), 1);
                assert_eq!(entities[0].signals, keyword(17));
            } else {
                assert_eq!(notes.len(), 1);
                assert_eq!(notes[0].signals, keyword(17));
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn multiple_selected_backends_keep_outer_rrf_when_only_one_contributes() {
    let shared = Uuid::from_u128(1);
    for failed in [false, true] {
        let mut registry = BackendRegistry::new();
        registry.register(backend_id("alpha"), memory_runtime());
        registry.register(backend_id("beta"), memory_runtime());
        let hit = entity_hit(shared, RankScoreKind::Union, vector(321));
        let mut coordinator = SubstrateCoordinator::new(registry)
            .with_entity_hits_override(HashMap::from([
                ("alpha".into(), vec![hit.clone()]),
                ("beta".into(), vec![]),
            ]))
            .with_note_hits_override(HashMap::from([
                ("alpha".into(), vec![as_note(hit)]),
                ("beta".into(), vec![]),
            ]));
        if failed {
            coordinator = coordinator.with_failing_backend("beta");
        }
        for kind in ["entity", "note"] {
            let request = request(serde_json::json!({"kind":kind,"query":"candidate","limit":10}));
            let (entities, notes, per_backend) = coordinator
                .fan_out_search(&request, &Namespace::local())
                .await;
            assert_eq!(per_backend.len(), 2);
            assert_eq!(per_backend[1].error.is_some(), failed);
            assert!(per_backend[1].hits.is_empty());
            assert!(per_backend[1].note_hits.is_empty());
            if kind == "entity" {
                assert_eq!(entities.len(), 1);
                assert_eq!(entities[0].score.to_raw(), 70_409_300);
                assert_eq!(entities[0].rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(entities[0].signals, vector(321));
            } else {
                assert_eq!(notes.len(), 1);
                assert_eq!(notes[0].score.to_raw(), 70_409_300);
                assert_eq!(notes[0].rank_score_kind, RankScoreKind::Rrf);
                assert_eq!(notes[0].signals, vector(321));
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finalization_preserves_backend_rank_kind_and_signals() {
    let coordinator = SubstrateCoordinator::single(memory_runtime());
    let entity = entity_hit(Uuid::from_u128(1), RankScoreKind::Vector, vector(17));
    let note = as_note(entity_hit(
        Uuid::from_u128(2),
        RankScoreKind::Weighted,
        keyword(0),
    ));
    let mut entities = vec![entity.clone()];
    let mut notes = vec![note.clone()];
    let request = request(serde_json::json!({"kind":"entity","query":"candidate","limit":10}));
    coordinator
        .finalize_search_hits(&mut entities, &mut notes, &request, &Namespace::local())
        .await;
    assert_eq!(entities.len(), 1);
    assert_eq!(notes.len(), 1);
    assert_entity_eq(&entities[0], &entity);
    assert_note_eq(&notes[0], &note);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn one_selected_backend_preserves_real_entity_and_note_evidence() {
    let runtime = memory_runtime();
    let namespace = Namespace::local();
    let token = runtime.authorize(namespace.clone()).unwrap();
    runtime
        .create_entity(
            &token,
            "concept",
            None,
            "rankingprobe",
            Some("rankingprobe entity"),
            None,
            vec![],
        )
        .await
        .unwrap();
    runtime
        .create_note(
            &token,
            "observation",
            Some("rankingprobe"),
            "rankingprobe note",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let mut registry = BackendRegistry::new();
    for (id, kind) in [
        ("entities", SubstrateKind::Entity),
        ("notes", SubstrateKind::Note),
    ] {
        registry
            .register_with_served_kinds(
                backend_id(id),
                Arc::clone(&runtime),
                Some(BTreeSet::from([kind])),
            )
            .unwrap();
    }
    let coordinator = SubstrateCoordinator::new(registry);
    assert!(!coordinator.is_single_backend());
    let entity_request =
        request(serde_json::json!({"kind":"entity","query":"rankingprobe","limit":10}));
    let expected = runtime
        .hybrid_search_outcome(&token, "rankingprobe", 100, None, None, &[], None)
        .await
        .unwrap();
    let (entities, _, backends) = coordinator
        .fan_out_search(&entity_request, &namespace)
        .await;
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].backend_id.as_str(), "entities");
    assert!(backends[0].error.is_none());
    assert_eq!(entities.len(), 1);
    assert_eq!(expected.hits.len(), 1);
    assert!(expected.hits[0].signals.keyword_score.is_some());
    assert_entity_eq(&entities[0], &expected.hits[0]);
    let note_request =
        request(serde_json::json!({"kind":"note","query":"rankingprobe","limit":10}));
    let expected = runtime
        .search_notes_outcome(
            &token,
            "rankingprobe",
            100,
            None,
            note_request.include_superseded(),
            &[],
            None,
        )
        .await
        .unwrap();
    let (_, notes, backends) = coordinator.fan_out_search(&note_request, &namespace).await;
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].backend_id.as_str(), "notes");
    assert!(backends[0].error.is_none());
    assert_eq!(notes.len(), 1);
    assert_eq!(expected.hits.len(), 1);
    assert!(expected.hits[0].signals.keyword_score.is_some());
    assert_note_eq(&notes[0], &expected.hits[0]);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finalization_quantizes_floor_once_and_keeps_equality() {
    let coordinator = SubstrateCoordinator::single(memory_runtime());
    let mut entities: Vec<_> = [(1, 12), (2, 11), (3, 10)]
        .into_iter()
        .map(|(id, raw)| SearchHit {
            score: DeterministicScore::from_raw(raw),
            ..entity_hit(Uuid::from_u128(id), RankScoreKind::Keyword, keyword(raw))
        })
        .collect();
    let mut notes = entities.iter().cloned().map(as_note).collect();
    let request = request(
        serde_json::json!({"kind":"entity","query":"candidate","min_score":11.25 / 4_294_967_296.0,"limit":2}),
    );
    coordinator
        .finalize_search_hits(&mut entities, &mut notes, &request, &Namespace::local())
        .await;
    assert_eq!(
        entities
            .iter()
            .map(|hit| hit.score.to_raw())
            .collect::<Vec<_>>(),
        vec![12, 11]
    );
    assert_eq!(
        notes
            .iter()
            .map(|hit| hit.score.to_raw())
            .collect::<Vec<_>>(),
        vec![12, 11]
    );
}

async fn seed_timestamps(
    runtime: &KhiveRuntime,
    id: Uuid,
    note_id: Uuid,
    created: i64,
    updated: i64,
) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut entity = Entity::new(Namespace::LOCAL, "concept", "candidate");
    entity.id = id;
    entity.created_at = created;
    entity.updated_at = updated;
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
    let mut note = Note::new(Namespace::LOCAL, "observation", "candidate");
    note.id = note_id;
    note.created_at = created;
    note.updated_at = updated;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn coordinator_time_order_applies_floor_before_limit_for_both_substrates() {
    let runtime = memory_runtime();
    let entities = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    let notes = [
        Uuid::from_u128(11),
        Uuid::from_u128(12),
        Uuid::from_u128(13),
    ];
    for (index, created, updated) in [(0, 100, 300), (1, 200, 200), (2, 400, 400)] {
        seed_timestamps(&runtime, entities[index], notes[index], created, updated).await;
    }
    let mut registry = BackendRegistry::new();
    registry.register(backend_id("alpha"), Arc::clone(&runtime));
    registry.register(backend_id("beta"), memory_runtime());
    let entity_hits: Vec<_> = entities
        .into_iter()
        .map(|id| entity_hit(id, RankScoreKind::Keyword, keyword(17)))
        .collect();
    let note_hits: Vec<_> = notes
        .into_iter()
        .map(|id| as_note(entity_hit(id, RankScoreKind::Keyword, keyword(17))))
        .collect();
    let coordinator = SubstrateCoordinator::new(registry)
        .with_entity_hits_override(HashMap::from([
            ("alpha".into(), entity_hits.clone()),
            ("beta".into(), entity_hits[..2].to_vec()),
        ]))
        .with_note_hits_override(HashMap::from([
            ("alpha".into(), note_hits.clone()),
            ("beta".into(), note_hits[..2].to_vec()),
        ]));
    let service = SubstrateCoordinatorService::new(coordinator);
    for (order, expected_index) in [("created_at", 1), ("updated_at", 0)] {
        for kind in ["entity", "note"] {
            let request = request(
                serde_json::json!({"kind":kind,"query":"candidate","order_by":order,"min_score":0.03,"limit":1}),
            );
            let result = service
                .fan_out_search(&request, &Namespace::local(), &[])
                .await;
            assert!(!result.partial);
            assert_eq!(result.per_backend.len(), 2);
            assert!(result.per_backend.iter().all(|row| row.error.is_none()));
            if kind == "entity" {
                assert_eq!(result.entity_hits.len(), 1);
                assert_eq!(result.entity_hits[0].entity_id, entities[expected_index]);
                assert_eq!(result.per_backend[0].entity_hits.len(), 3);
                assert_eq!(result.per_backend[1].entity_hits.len(), 2);
                assert_eq!(
                    result.entity_created_at[&entities[expected_index]],
                    if expected_index == 1 { 200 } else { 100 }
                );
            } else {
                assert_eq!(result.note_hits.len(), 1);
                assert_eq!(result.note_hits[0].note_id, notes[expected_index]);
                assert_eq!(result.per_backend[0].note_hits.len(), 3);
                assert_eq!(result.per_backend[1].note_hits.len(), 2);
                assert_eq!(
                    result.note_created_at[&notes[expected_index]],
                    if expected_index == 1 { 200 } else { 100 }
                );
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn time_order_preserves_rank_and_uuid_ties_and_places_missing_last() {
    let runtime = memory_runtime();
    for id in [1, 2] {
        seed_timestamps(
            &runtime,
            Uuid::from_u128(id),
            Uuid::from_u128(id + 10),
            100,
            100,
        )
        .await;
    }
    let coordinator = SubstrateCoordinator::single(runtime);
    for order in ["created_at", "updated_at"] {
        let mut entities: Vec<_> = [3, 1, 2]
            .into_iter()
            .map(|id| {
                let mut hit = entity_hit(Uuid::from_u128(id), RankScoreKind::Rrf, keyword(17));
                if id == 3 {
                    hit.score = DeterministicScore::from_raw(1_000_000_000);
                }
                hit
            })
            .collect();
        let mut notes = [13, 11, 12]
            .into_iter()
            .map(|id| {
                let mut hit = as_note(entity_hit(
                    Uuid::from_u128(id),
                    RankScoreKind::Rrf,
                    keyword(17),
                ));
                if id == 13 {
                    hit.score = DeterministicScore::from_raw(1_000_000_000);
                }
                hit
            })
            .collect();
        let request = request(
            serde_json::json!({"kind":"entity","query":"candidate","order_by":order,"limit":10}),
        );
        coordinator
            .finalize_search_hits(&mut entities, &mut notes, &request, &Namespace::local())
            .await;
        assert_eq!(
            entities.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
            [1, 2, 3].map(Uuid::from_u128)
        );
        assert_eq!(
            notes.iter().map(|hit| hit.note_id).collect::<Vec<_>>(),
            [11, 12, 13].map(Uuid::from_u128)
        );
    }
}
