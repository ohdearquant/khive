use std::sync::Arc;

use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RankScoreKind, RuntimeConfig, SearchSource,
    StorageBackend,
};
use khive_score::{rrf_score, DeterministicScore};
use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest};

fn runtime() -> (KhiveRuntime, NamespaceToken) {
    let backend = Arc::new(StorageBackend::memory().expect("in-memory backend"));
    backend.prepare_core_schema().expect("core schema");
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:search-ranking-evidence".into()),
            ..RuntimeConfig::no_embeddings()
        },
    );
    let token = runtime.authorize(Namespace::local()).expect("local token");
    (runtime, token)
}

fn lexical_request(query: &str, record_kind: &str) -> TextSearchRequest {
    TextSearchRequest {
        query: query.into(),
        mode: TextQueryMode::Plain,
        filter: Some(TextFilter {
            namespaces: vec![Namespace::LOCAL.into()],
            record_kinds: vec![record_kind.into()],
            ..TextFilter::default()
        }),
        top_k: 40,
        snippet_chars: 200,
    }
}

#[tokio::test]
async fn note_search_retains_keyword_evidence_through_salience_weighting() {
    for (salience, divisor) in [(0.0, 2), (1.0, 1)] {
        let (runtime, token) = runtime();
        let note = runtime
            .create_note(
                &token,
                "observation",
                None,
                "Prismmarker lexical evidence",
                Some(salience),
                None,
                vec![],
            )
            .await
            .expect("create note");
        let raw = runtime
            .text_for_notes(&token)
            .expect("note index")
            .search(lexical_request("Prismmarker", "observation"))
            .await
            .expect("lexical baseline");
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].subject_id, note.id);

        for _ in 0..3 {
            let hits = runtime
                .search_notes(
                    &token,
                    "Prismmarker",
                    None,
                    10,
                    Some("observation"),
                    false,
                    &[],
                    None,
                )
                .await
                .expect("note search");
            assert_eq!(hits.len(), 1);
            let hit = &hits[0];
            assert_eq!(hit.note_id, note.id);
            assert_eq!(hit.source, SearchSource::Text);
            assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
            assert_eq!(hit.score, rrf_score(1, 60) / divisor);
            assert_eq!(hit.signals.keyword_score, Some(raw[0].score));
            assert_eq!(hit.signals.vector_similarity, None);
        }
    }
}

#[tokio::test]
async fn entity_search_retains_keyword_evidence_separately_from_title_boost() {
    let (runtime, token) = runtime();
    let entity = runtime
        .create_entity(
            &token,
            "concept",
            None,
            "Prismmarker",
            Some("Lexical evidence remains separate from the title boost"),
            None,
            vec![],
        )
        .await
        .expect("create entity");
    let raw = runtime
        .text(&token)
        .expect("entity index")
        .search(lexical_request("Prismmarker", "concept"))
        .await
        .expect("lexical baseline");
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].subject_id, entity.id);

    let hits = runtime
        .hybrid_search(
            &token,
            "Prismmarker",
            None,
            10,
            Some("concept"),
            None,
            &[],
            None,
        )
        .await
        .expect("entity search");
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit.entity_id, entity.id);
    assert_eq!(hit.source, SearchSource::Text);
    assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
    assert_eq!(
        hit.score,
        rrf_score(1, 10) + DeterministicScore::from_f64(0.5)
    );
    assert_eq!(hit.signals.keyword_score, Some(raw[0].score));
    assert_eq!(hit.signals.vector_similarity, None);
}
