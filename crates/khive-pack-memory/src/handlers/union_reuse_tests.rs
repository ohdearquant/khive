use super::*;

fn id(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("literal UUID")
}

fn raw_pairs(hits: &[(Uuid, DeterministicScore)]) -> Vec<(Uuid, i64)> {
    hits.iter()
        .map(|(id, score)| (*id, score.to_raw()))
        .collect()
}

fn vector(id: Uuid, raw: i64, rank: u32) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank,
    }
}

fn text(id: Uuid, raw: i64, title: &str) -> TextSearchHit {
    TextSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank: 1,
        title: Some(title.to_owned()),
        snippet: Some(format!("{title} snippet")),
    }
}

fn candidates(
    text_hits: Vec<TextSearchHit>,
    models: Vec<(String, Vec<VectorSearchHit>)>,
) -> RecallCandidateSet {
    RecallCandidateSet {
        namespace: "local".to_owned(),
        text_hits,
        vector_hits_per_model: models,
        visible_namespaces: vec!["local".to_owned()],
        ann_degraded: false,
        ann_degraded_reason: None,
        session_unmet_models: Vec::new(),
        timings: RecallStageTimings::default(),
    }
}

#[test]
fn union_keeps_literal_raw_maxima_uuid_ties_and_empty_sources() {
    let one = id("00000000-0000-0000-0000-000000000001");
    let two = id("00000000-0000-0000-0000-000000000002");
    let three = id("00000000-0000-0000-0000-000000000003");
    let four = id("00000000-0000-0000-0000-000000000004");
    let high = 1_152_921_504_606_846_979;
    let sources = vec![
        vec![
            (two, DeterministicScore::from_raw(high)),
            (one, DeterministicScore::from_raw(high - 1)),
            (four, DeterministicScore::from_raw(-17)),
            (three, DeterministicScore::from_raw(-1)),
            (one, DeterministicScore::from_raw(high - 2)),
        ],
        Vec::new(),
        vec![
            (three, DeterministicScore::ZERO),
            (one, DeterministicScore::from_raw(high)),
            (two, DeterministicScore::from_raw(high - 1)),
        ],
    ];
    let expected = vec![(one, high), (two, high), (three, 0), (four, -17)];
    assert_eq!(
        raw_pairs(&combine_vector_sources_union(sources.clone())),
        expected
    );

    let reversed = sources
        .into_iter()
        .rev()
        .map(|source| source.into_iter().rev().collect())
        .collect();
    assert_eq!(raw_pairs(&combine_vector_sources_union(reversed)), expected);
    assert!(combine_vector_sources_union(Vec::new()).is_empty());
    assert!(combine_vector_sources_union(vec![Vec::new(), Vec::new()]).is_empty());
    assert_eq!(
        raw_pairs(&combine_vector_sources_union(vec![vec![
            (two, DeterministicScore::from_raw(7)),
            (one, DeterministicScore::from_raw(7)),
            (one, DeterministicScore::from_raw(6)),
        ]])),
        vec![(one, 7), (two, 7)]
    );
}

#[test]
fn vector_only_recall_keeps_union_order_scores_and_source_metadata() {
    let one = id("00000000-0000-0000-0000-000000000001");
    let two = id("00000000-0000-0000-0000-000000000002");
    let three = id("00000000-0000-0000-0000-000000000003");
    let hidden = id("00000000-0000-0000-0000-000000000004");
    let text_only = id("00000000-0000-0000-0000-000000000005");
    let candidates = candidates(
        vec![
            text(three, 9_999, "ignored"),
            text(text_only, 10_000, "text only"),
        ],
        vec![
            (
                "model-a".to_owned(),
                vec![
                    vector(two, 900, 1),
                    vector(one, 100, 2),
                    vector(three, -50, 3),
                    vector(hidden, 99_999, 4),
                ],
            ),
            (
                "model-b".to_owned(),
                vec![
                    vector(one, 900, 1),
                    vector(two, 800, 2),
                    vector(three, -100, 3),
                ],
            ),
        ],
    );
    let memory_ids = HashSet::from([one, two, three, text_only]);
    let config = RecallConfig {
        fuse_strategy: FusionStrategy::VectorOnly,
        ..RecallConfig::default()
    };
    let hits = fuse_candidates(&candidates, &memory_ids, &config, 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| (hit.entity_id, hit.score.to_raw()))
            .collect::<Vec<_>>(),
        vec![(one, 900), (two, 900), (three, -50)]
    );
    for hit in &hits {
        assert!(matches!(hit.source, SearchSource::Vector));
        assert!(matches!(
            hit.rank_score_kind,
            khive_runtime::RankScoreKind::Vector
        ));
        assert!(hit.title.is_none());
        assert!(hit.snippet.is_none());
    }
    let limited = fuse_candidates(&candidates, &memory_ids, &config, 2).unwrap();
    assert_eq!(
        limited.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        vec![one, two]
    );
}

#[test]
fn two_model_weighted_recall_keeps_text_slot_and_literal_fixed_point_scores() {
    let one = id("00000000-0000-0000-0000-000000000001");
    let two = id("00000000-0000-0000-0000-000000000002");
    let three = id("00000000-0000-0000-0000-000000000003");
    let hidden = id("00000000-0000-0000-0000-000000000004");
    let text_only = id("00000000-0000-0000-0000-000000000005");
    let candidates = candidates(
        vec![
            text(one, 10, "one"),
            text(three, 20, "three"),
            text(text_only, 30, "text only"),
        ],
        vec![
            (
                "model-a".to_owned(),
                vec![
                    vector(one, 0, 1),
                    vector(two, 8, 2),
                    vector(hidden, 99_999, 3),
                ],
            ),
            (
                "model-b".to_owned(),
                vec![vector(one, 6, 1), vector(three, 6, 2), vector(two, 4, 3)],
            ),
        ],
    );
    let memory_ids = HashSet::from([one, two, three, text_only]);
    let config = RecallConfig {
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
        ..RecallConfig::default()
    };
    let hits = fuse_candidates(&candidates, &memory_ids, &config, 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| (hit.entity_id, hit.score.to_raw()))
            .collect::<Vec<_>>(),
        vec![
            (two, 2_147_483_648),
            (text_only, 2_147_483_648),
            (three, 1_073_741_824),
            (one, 0)
        ]
    );
    assert!(matches!(hits[0].source, SearchSource::Vector));
    assert!(matches!(hits[1].source, SearchSource::Text));
    assert!(matches!(hits[2].source, SearchSource::Both));
    assert!(matches!(hits[3].source, SearchSource::Both));
    assert_eq!(hits[0].title, None);
    assert_eq!(hits[1].title.as_deref(), Some("text only"));
    assert_eq!(hits[2].snippet.as_deref(), Some("three snippet"));
    for hit in &hits {
        assert!(matches!(
            hit.rank_score_kind,
            khive_runtime::RankScoreKind::Weighted
        ));
    }
    let limited = fuse_candidates(&candidates, &memory_ids, &config, 3).unwrap();
    assert_eq!(
        limited.iter().map(|hit| hit.entity_id).collect::<Vec<_>>(),
        vec![two, text_only, three]
    );
}
