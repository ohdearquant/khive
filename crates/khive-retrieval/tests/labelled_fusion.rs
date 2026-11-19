use khive_retrieval::hybrid::{
    combine_best_ranked_evidence, combine_leg_first_appearance, fuse_labelled, HitLabel,
};
use khive_retrieval::{reciprocal_rank_fusion, SearchSignals, SearchSource};
use khive_score::DeterministicScore;

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

fn both(keyword_raw: i64, vector_raw: i64) -> SearchSignals {
    SearchSignals {
        vector_similarity: vector(vector_raw).vector_similarity,
        keyword_score: keyword(keyword_raw).keyword_score,
    }
}

fn hit(
    id: u8,
    rank: usize,
    signals: SearchSignals,
    source: SearchSource,
    title: Option<&str>,
) -> (u8, HitLabel) {
    let label = HitLabel {
        rank,
        signals,
        source,
        title: title.map(str::to_string),
        snippet: None,
    };
    (id, label)
}

fn label_of(fused: &[(u8, DeterministicScore, HitLabel)], id: u8) -> HitLabel {
    let found = fused.iter().find(|(held, _, _)| *held == id);
    found.expect("id is fused").2.clone()
}

fn fuse(arms: Vec<Vec<(u8, HitLabel)>>) -> Vec<(u8, DeterministicScore, HitLabel)> {
    fuse_labelled(arms, 60, combine_best_ranked_evidence)
}

#[test]
fn fuse_labelled_scores_and_order_equal_reciprocal_rank_fusion() {
    // Equal scores across arms, a repeated id inside one arm, and ids seen by one arm only.
    let id_arms: [&[u8]; 3] = [&[1, 2, 1, 3], &[2, 1, 4], &[5, 5, 3]];
    for k in [10, 60] {
        let mut labelled = Vec::new();
        let mut plain = Vec::new();
        for ids in id_arms {
            let mut with_labels = Vec::new();
            let mut with_scores = Vec::new();
            for (rank, id) in ids.iter().enumerate() {
                let signals = SearchSignals::default();
                with_labels.push(hit(*id, rank, signals, SearchSource::Text, None));
                with_scores.push((*id, DeterministicScore::ZERO));
            }
            labelled.push(with_labels);
            plain.push(with_scores);
        }

        let fused = fuse_labelled(labelled, k, combine_best_ranked_evidence);
        let rows: Vec<_> = fused.iter().map(|(id, score, _)| (*id, *score)).collect();

        assert_eq!(rows, reciprocal_rank_fusion(plain, k));
    }
}

#[test]
fn fuse_labelled_combines_appearances_in_arm_then_position_order() {
    // A label type that is not a hit label: the combine rule alone decides the result.
    let first = vec![("x", 1_u32), ("y", 2), ("x", 3)];
    let second = vec![("x", 4)];
    let combine = |held: u32, incoming: u32| held * 10 + incoming;

    let fused = fuse_labelled(vec![first, second], 60, combine);

    let labels: Vec<_> = fused.iter().map(|(id, _, label)| (*id, *label)).collect();
    assert_eq!(labels, vec![("x", 134), ("y", 2)]);
}

#[test]
fn fuse_labelled_returns_the_full_order_without_a_cut() {
    let ids: Vec<u8> = (1..=40).collect();
    let signals = SearchSignals::default();
    let mut arm = Vec::new();
    for (rank, id) in ids.iter().enumerate() {
        arm.push(hit(*id, rank, signals, SearchSource::Text, None));
    }

    let fused = fuse(vec![arm]);

    let order: Vec<_> = fused.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(order, ids);
}

#[test]
fn leg_first_appearance_unions_the_legs_into_the_source() {
    let text = vec![
        hit(1, 0, keyword(5), SearchSource::Text, None),
        hit(2, 1, keyword(4), SearchSource::Text, None),
    ];
    let vector_arm = vec![
        hit(2, 0, vector(9), SearchSource::Vector, None),
        hit(3, 1, vector(8), SearchSource::Vector, None),
    ];

    let fused = fuse_labelled(vec![text, vector_arm], 10, combine_leg_first_appearance);

    let ids: Vec<_> = fused.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(ids, vec![2, 1, 3]);
    assert_eq!(fused[0].2.source, SearchSource::Both);
    assert_eq!(fused[1].2.source, SearchSource::Text);
    assert_eq!(fused[2].2.source, SearchSource::Vector);
    assert_eq!(fused[0].2.signals, both(4, 9));
}

#[test]
fn leg_first_appearance_ignores_a_repeat_from_a_leg_that_already_contributed() {
    let text = vec![
        hit(1, 0, keyword(5), SearchSource::Text, None),
        hit(1, 1, keyword(6), SearchSource::Text, Some("late")),
    ];
    let vector_arm = vec![hit(1, 0, vector(9), SearchSource::Vector, None)];

    let fused = fuse_labelled(vec![text, vector_arm], 10, combine_leg_first_appearance);

    assert_eq!(fused.len(), 1);
    let only = label_of(&fused, 1);
    assert_eq!(only.title, None);
    assert_eq!(only.signals, both(5, 9));
    assert_eq!(only.source, SearchSource::Both);
}

#[test]
fn best_ranked_evidence_keeps_the_best_rank_and_the_earlier_arm_on_a_tie() {
    let first = vec![
        hit(9, 0, keyword(1), SearchSource::Text, None),
        hit(5, 1, keyword(10), SearchSource::Text, None),
    ];
    let second = vec![
        hit(8, 0, vector(2), SearchSource::Vector, None),
        hit(5, 1, vector(20), SearchSource::Vector, Some("late")),
    ];
    let top = hit(5, 0, keyword(30), SearchSource::Text, Some("top"));
    let third = vec![top];

    let tie = label_of(&fuse(vec![first.clone(), second.clone()]), 5);
    assert_eq!(tie.signals, keyword(10));
    assert_eq!(tie.rank, 1);
    assert_eq!(tie.source, SearchSource::Both);
    assert_eq!(tie.title.as_deref(), Some("late"));

    let best = label_of(&fuse(vec![first, second, third]), 5);
    assert_eq!(best.signals, keyword(30));
    assert_eq!(best.rank, 0);
    assert_eq!(best.title.as_deref(), Some("late"));
}
