use super::*;

fn request(top_k: u32, snippet_chars: usize) -> TextSearchRequest {
    TextSearchRequest {
        query: "needle".to_string(),
        mode: TextQueryMode::Plain,
        filter: Some(ns_filter("test_ns")),
        top_k,
        snippet_chars,
    }
}

fn options(gather_limit: Option<u32>) -> TextSearchOptions {
    TextSearchOptions {
        gather_mode: TextGatherMode::RankWithinCap,
        gather_limit,
    }
}

#[tokio::test]
async fn bounded_ranking_does_not_expand_duplicate_ids_outside_exact_filters() {
    for case in ["namespace", "substrate", "short_kind"] {
        let store = setup_trigram_store(case);
        let id = Uuid::from_u128(10);
        let mut local = make_record_kind_document(id, "ai", "local title", "zircon local evidence");
        local.namespace = "local".to_string();
        let mut foreign =
            make_record_kind_document(id, "memory", "foreign title", "zircon foreign evidence");
        foreign.namespace = "foreign".to_string();
        foreign.kind = SubstrateKind::Entity;
        store.upsert_document(local).await.unwrap();
        store.upsert_document(foreign).await.unwrap();
        assert_eq!(store.count(TextFilter::default()).await.unwrap(), 2);
        let mut filter = TextFilter::default();
        match case {
            "namespace" => filter.namespaces = vec!["local".to_string()],
            "substrate" => filter.kinds = vec![SubstrateKind::Note],
            "short_kind" => filter.record_kinds = vec!["ai".to_string()],
            _ => unreachable!(),
        }
        let hits = store
            .search_with_options(
                TextSearchRequest {
                    query: "zircon".to_string(),
                    filter: Some(filter),
                    ..request(2, 64)
                },
                options(Some(2)),
            )
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "{case}: a gathered UUID must not expand into another physical row"
        );
        assert_eq!(hits[0].subject_id, id);
        assert_eq!(hits[0].title.as_deref(), Some("local title"));
        assert_eq!(hits[0].snippet.as_deref(), Some("zircon local evidence"));
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].score, DeterministicScore::from_f64(1.0));
    }
}
