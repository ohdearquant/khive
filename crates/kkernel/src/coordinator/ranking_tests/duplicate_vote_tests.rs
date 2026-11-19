#[test]
fn outer_rrf_counts_a_repeated_entity_id_once_per_list() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let make = |id| entity_hit(id, RankScoreKind::Keyword, keyword(17));

    let merged = rrf_merge_entity_hits(vec![vec![make(a), make(a), make(b)]], 10);
    assert_eq!(merged.len(), 2);
    assert_eq!(entity_score(&merged, a), rrf_score(1, 60));
    assert_eq!(entity_score(&merged, b), rrf_score(3, 60));

    let without_b = rrf_merge_entity_hits(vec![vec![make(a)]], 10);
    assert_eq!(entity_score(&merged, a), entity_score(&without_b, a));

    // The skip is per list: the same id still votes once in every list it appears in.
    let first = vec![make(a), make(a), make(b)];
    let second = vec![make(b), make(a)];
    let merged = rrf_merge_entity_hits(vec![first, second], 10);
    let a_total = rrf_score(1, 60) + rrf_score(2, 60);
    let b_total = rrf_score(3, 60) + rrf_score(1, 60);
    assert_eq!(entity_score(&merged, a), a_total);
    assert_eq!(entity_score(&merged, b), b_total);
}

#[test]
fn outer_rrf_counts_a_repeated_note_id_once_per_list() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let make = |id| as_note(entity_hit(id, RankScoreKind::Keyword, keyword(17)));

    let merged = rrf_merge_note_hits(vec![vec![make(a), make(a), make(b)]], 10);
    assert_eq!(merged.len(), 2);
    assert_eq!(note_score(&merged, a), rrf_score(1, 60));
    assert_eq!(note_score(&merged, b), rrf_score(3, 60));

    let without_b = rrf_merge_note_hits(vec![vec![make(a)]], 10);
    assert_eq!(note_score(&merged, a), note_score(&without_b, a));

    // The skip is per list: the same id still votes once in every list it appears in.
    let first = vec![make(a), make(a), make(b)];
    let second = vec![make(b), make(a)];
    let merged = rrf_merge_note_hits(vec![first, second], 10);
    let a_total = rrf_score(1, 60) + rrf_score(2, 60);
    let b_total = rrf_score(3, 60) + rrf_score(1, 60);
    assert_eq!(note_score(&merged, a), a_total);
    assert_eq!(note_score(&merged, b), b_total);
}

#[test]
fn outer_rrf_keeps_metadata_of_a_repeated_id_in_one_list() {
    let a = Uuid::from_u128(1);
    let mut first = entity_hit(a, RankScoreKind::Keyword, keyword(17));
    first.title = None;
    first.snippet = None;
    let second = entity_hit(a, RankScoreKind::Vector, vector(5));
    let list = vec![first, second];
    let note_list: Vec<NoteSearchHit> = list.iter().cloned().map(as_note).collect();

    let entities = rrf_merge_entity_hits(vec![list], 10);
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].score, rrf_score(1, 60));
    assert_eq!(entities[0].signals, keyword(17));
    assert_eq!(entities[0].source, SearchSource::Both);
    assert_eq!(entities[0].title.as_deref(), Some("candidate"));
    assert_eq!(entities[0].snippet.as_deref(), Some("retained snippet"));

    let notes = rrf_merge_note_hits(vec![note_list], 10);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].score, rrf_score(1, 60));
    assert_eq!(notes[0].signals, keyword(17));
    assert_eq!(notes[0].source, SearchSource::Both);
    assert_eq!(notes[0].title.as_deref(), Some("candidate"));
    assert_eq!(notes[0].snippet.as_deref(), Some("retained snippet"));
}
