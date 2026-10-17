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
