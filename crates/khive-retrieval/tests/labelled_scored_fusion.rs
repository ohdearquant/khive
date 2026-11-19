use std::collections::HashMap;

use khive_retrieval::hybrid::fuse_labelled_scored;
use khive_retrieval::{fuse_search_results, FusionStrategy, HybridConfig};
use khive_score::DeterministicScore;

type Row = (u8, DeterministicScore, String);

// A scenario is a list of arms separated by `|`, and a row is `id:score`. An empty arm keeps its
// slot, because the weighted strategy reads the position of each arm.
const SCENARIOS: &[(&str, &str)] = &[
    (
        "ties across and inside arms",
        "1:0.9 2:0.8 3:0.8 | 4:0.7 5:0.7 6:0.1",
    ),
    (
        "an id repeated inside one arm",
        "1:0.9 2:0.8 1:0.7 3:0.6 | 2:0.9 4:0.5 2:0.4",
    ),
    (
        "every id in both arms",
        "1:0.9 2:0.8 3:0.7 | 3:0.9 2:0.8 1:0.7",
    ),
    ("empty first arm", " | 7:0.9 8:0.5 9:0.2"),
    ("empty second arm", "7:0.9 8:0.5 9:0.2 | "),
    ("only empty arms", " | "),
    ("one arm only", "1:0.9 2:0.4 3:0.3"),
    (
        "three arms",
        "1:0.9 2:0.6 | 2:0.9 3:0.5 | 3:0.9 1:0.2 4:0.1",
    ),
    ("empty first arm of three", " | 1:0.9 2:0.5 | 2:0.9 3:0.1"),
    ("empty middle arm of three", "1:0.9 | | 1:0.8 2:0.7"),
    ("empty last arm of three", "1:0.9 | 2:0.8 | "),
];

// Each appearance is labelled `arm.position`, so a label shows which appearances it folds.
fn labelled(spec: &str) -> Vec<Vec<Row>> {
    let mut arms = Vec::new();
    for (arm_index, arm) in spec.split('|').enumerate() {
        let mut rows = Vec::new();
        for (position, row) in arm.split_whitespace().enumerate() {
            let (id, score) = row.split_once(':').expect("a row is id:score");
            let id: u8 = id.parse().expect("an id is a small integer");
            let score: f64 = score.parse().expect("a score is a number");
            let label = format!("{arm_index}.{position}");
            rows.push((id, DeterministicScore::from_f64(score), label));
        }
        arms.push(rows);
    }
    arms
}

// Neither commutative nor idempotent, so the order and the number of calls both show.
fn join_labels(held: String, incoming: String) -> String {
    format!("{held}+{incoming}")
}

// The combine rule folded over every appearance of an id, in arm order then position order.
fn expected_labels(arms: &[Vec<Row>]) -> HashMap<u8, String> {
    let mut seen: HashMap<u8, Vec<String>> = HashMap::new();
    for (id, _, label) in arms.iter().flatten() {
        seen.entry(*id).or_default().push(label.clone());
    }
    let mut folded = HashMap::new();
    for (id, labels) in seen {
        let label = labels.into_iter().reduce(join_labels);
        folded.insert(id, label.expect("an id has an appearance"));
    }
    folded
}

fn raw_rows(fused: &[Row]) -> Vec<(u8, i64)> {
    let mut rows = Vec::new();
    for (id, score, _) in fused {
        rows.push((*id, score.to_raw()));
    }
    rows
}

fn sorted_ids(fused: &[Row]) -> Vec<u8> {
    let mut ids = Vec::new();
    for (id, _, _) in fused {
        ids.push(*id);
    }
    ids.sort_unstable();
    ids
}

fn label_of(fused: &[Row], id: u8) -> &str {
    let found = fused.iter().find(|(held, _, _)| *held == id);
    &found.expect("id is returned").2
}

fn assert_matches_scoring(name: &str, arms: &[Vec<Row>], config: &HybridConfig) {
    let mut plain = Vec::new();
    for arm in arms {
        let mut rows = Vec::new();
        for (id, score, _) in arm {
            rows.push((*id, *score));
        }
        plain.push(rows);
    }
    let scored = fuse_search_results(plain, config).unwrap();
    let fused = fuse_labelled_scored(arms.to_vec(), config, join_labels).unwrap();

    let mut want = Vec::new();
    for (id, score) in &scored {
        want.push((*id, score.to_raw()));
    }
    let got = raw_rows(&fused);
    assert_eq!(got, want, "{name}: rows");

    let expected = expected_labels(arms);
    for (id, _, label) in &fused {
        assert_eq!(label, &expected[id], "{name}: label of id {id}");
    }
}

// Runs every scenario with no cut, with `top_k` below the result size and with a score floor.
// `floor` is chosen per strategy so that it removes the tail of some scenario.
fn check_strategy(strategy: FusionStrategy, floor: f64) {
    let uncut = HybridConfig::new(50)
        .with_fusion_strategy(strategy)
        .with_weights(0.8, 0.2);
    let top_two = HybridConfig {
        top_k: 2,
        ..uncut.clone()
    };
    let floor = DeterministicScore::from_f64(floor);
    let floored = uncut.clone().with_min_score(floor);

    assert_matches_scoring("no arms", &[], &uncut);
    for &(name, spec) in SCENARIOS {
        let arms = labelled(spec);
        for config in [&uncut, &top_two, &floored] {
            assert_matches_scoring(name, &arms, config);
        }
    }
}

#[test]
fn fuse_labelled_scored_reciprocal_rank_equals_fuse_search_results() {
    check_strategy(FusionStrategy::rrf(), 0.016);
}

#[test]
fn fuse_labelled_scored_weighted_equals_fuse_search_results() {
    // The three-arm scenarios fall back to reciprocal rank, as `fuse_search_results` does.
    check_strategy(FusionStrategy::weighted(vec![0.8, 0.2]), 0.5);
}

#[test]
fn fuse_labelled_scored_union_equals_fuse_search_results() {
    check_strategy(FusionStrategy::union(), 0.5);
}

#[test]
fn fuse_labelled_scored_vector_only_equals_fuse_search_results() {
    check_strategy(FusionStrategy::VectorOnly, 0.5);
}

#[test]
fn fuse_labelled_scored_keyword_only_equals_fuse_search_results() {
    check_strategy(FusionStrategy::KeywordOnly, 0.5);
}

#[test]
fn fuse_labelled_scored_custom_falls_back_to_reciprocal_rank() {
    let custom = FusionStrategy::Custom {
        name: "decay_weighted".to_string(),
        params: serde_json::json!({}),
    };
    check_strategy(custom, 0.016);
}

#[test]
fn fuse_labelled_scored_combines_every_appearance_in_arm_then_position_order() {
    // Id 1 appears twice in the first arm and once in the second.
    let arms = labelled("1:0.9 2:0.8 1:0.7 | 3:0.9 1:0.8");
    let config = HybridConfig::new(10);

    let fused = fuse_labelled_scored(arms, &config, join_labels).unwrap();

    assert_eq!(fused.len(), 3);
    assert_eq!(label_of(&fused, 1), "0.0+0.2+1.1");
    assert_eq!(label_of(&fused, 2), "0.1");
    assert_eq!(label_of(&fused, 3), "1.0");
}

#[test]
fn fuse_labelled_scored_keeps_the_labels_of_arms_the_strategy_ignores() {
    // Vector only scores the first arm, yet id 1 also appears in the second arm that it ignores.
    let arms = labelled("1:0.9 2:0.8 | 1:0.7 3:0.6");
    let config = HybridConfig::new(10).with_fusion_strategy(FusionStrategy::VectorOnly);

    let fused = fuse_labelled_scored(arms, &config, join_labels).unwrap();

    assert_eq!(fused.len(), 2);
    assert_eq!(label_of(&fused, 1), "0.0+1.0");
    assert_eq!(label_of(&fused, 2), "0.1");
}

#[test]
fn fuse_labelled_scored_returns_only_the_ids_scoring_keeps() {
    // Reciprocal rank scores: ids 1 and 5 tie at 1/61, then id 2 at 1/62, id 3 at 1/63 and
    // id 4 at 1/64.
    let arms = labelled("1:0.9 2:0.8 3:0.7 4:0.6 | 5:0.9");

    let top_two = HybridConfig::new(2);
    let fused = fuse_labelled_scored(arms.clone(), &top_two, join_labels).unwrap();
    assert_eq!(sorted_ids(&fused), vec![1, 5]);

    let floor = DeterministicScore::from_f64(0.016);
    let floored = HybridConfig::new(10).with_min_score(floor);
    let fused = fuse_labelled_scored(arms, &floored, join_labels).unwrap();
    assert_eq!(sorted_ids(&fused), vec![1, 2, 5]);
}

#[test]
fn fuse_labelled_scored_preserves_errors_before_combining_labels() {
    use khive_fusion::FuseError;

    let config =
        HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted_rrf(10, vec![1.0]));
    let arms = labelled("1:0.9 | 1:0.8");
    let result = fuse_labelled_scored(arms, &config, |_, _| {
        panic!("invalid fusion must return before combining labels")
    });
    assert_eq!(
        result,
        Err(FuseError::WeightedRrfWeightCountMismatch {
            source_count: 2,
            weight_count: 1,
        })
    );
}
