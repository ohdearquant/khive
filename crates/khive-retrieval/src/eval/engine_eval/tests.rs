use super::*;

// ---- helpers ----

fn uuid(n: u64) -> Uuid {
    Uuid::from_u64_pair(0, n)
}

fn make_result(n: u64, label: RetrievalLabel) -> LabeledResult {
    LabeledResult {
        section_id: uuid(n),
        score: 1.0 / (n as f64 + 1.0),
        label,
    }
}

// ---- RetrievalLabel ----

#[test]
fn label_gain_values() {
    assert_eq!(RetrievalLabel::Decisive.gain(), 3.0);
    assert_eq!(RetrievalLabel::Supporting.gain(), 2.0);
    assert_eq!(RetrievalLabel::Background.gain(), 0.5);
    assert_eq!(RetrievalLabel::Irrelevant.gain(), 0.0);
    assert_eq!(RetrievalLabel::AdjacentWrong.gain(), -2.0);
}

#[test]
fn label_is_relevant() {
    assert!(RetrievalLabel::Decisive.is_relevant());
    assert!(RetrievalLabel::Supporting.is_relevant());
    assert!(!RetrievalLabel::Background.is_relevant());
    assert!(!RetrievalLabel::Irrelevant.is_relevant());
    assert!(!RetrievalLabel::AdjacentWrong.is_relevant());
}

#[test]
fn label_is_distractor() {
    assert!(RetrievalLabel::AdjacentWrong.is_distractor());
    assert!(!RetrievalLabel::Decisive.is_distractor());
    assert!(!RetrievalLabel::Irrelevant.is_distractor());
}

// ---- recall_at_k ----

#[test]
fn recall_at_k_all_relevant() {
    // 3 decisive results, k = 3 → recall = 1.0
    let results: Vec<LabeledResult> = (0..3)
        .map(|i| make_result(i, RetrievalLabel::Decisive))
        .collect();
    assert!((recall_at_k(&results, 3) - 1.0).abs() < 1e-9);
}

#[test]
fn recall_at_k_partial() {
    // 2 decisive at positions 0,1; 2 irrelevant at 2,3
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Decisive),
        make_result(2, RetrievalLabel::Irrelevant),
        make_result(3, RetrievalLabel::Irrelevant),
    ];
    // k=1: 1 of 2 decisive in top-1 → 0.5
    assert!((recall_at_k(&results, 1) - 0.5).abs() < 1e-9);
    // k=2: 2 of 2 decisive in top-2 → 1.0
    assert!((recall_at_k(&results, 2) - 1.0).abs() < 1e-9);
}

#[test]
fn recall_at_k_none_relevant_vacuously_one() {
    let results = vec![
        make_result(0, RetrievalLabel::Irrelevant),
        make_result(1, RetrievalLabel::Background),
    ];
    assert!((recall_at_k(&results, 5) - 1.0).abs() < 1e-9);
}

#[test]
fn recall_at_k_k_exceeds_length() {
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    // k=100 should be clamped to len=1
    assert!((recall_at_k(&results, 100) - 1.0).abs() < 1e-9);
}

// ---- precision_at_k ----

#[test]
fn precision_at_k_perfect() {
    let results: Vec<LabeledResult> = (0..5)
        .map(|i| make_result(i, RetrievalLabel::Decisive))
        .collect();
    assert!((precision_at_k(&results, 5) - 1.0).abs() < 1e-9);
}

#[test]
fn precision_at_k_half_relevant() {
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Irrelevant),
        make_result(2, RetrievalLabel::Supporting),
        make_result(3, RetrievalLabel::Irrelevant),
    ];
    // top-4: 2 relevant → 0.5
    assert!((precision_at_k(&results, 4) - 0.5).abs() < 1e-9);
}

#[test]
fn precision_at_k_zero_when_k_zero() {
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    assert_eq!(precision_at_k(&results, 0), 0.0);
}

#[test]
fn precision_at_k_zero_when_empty() {
    assert_eq!(precision_at_k(&[], 5), 0.0);
}

#[test]
fn precision_at_k_adjacent_wrong_not_counted() {
    let results = vec![
        make_result(0, RetrievalLabel::AdjacentWrong),
        make_result(1, RetrievalLabel::AdjacentWrong),
    ];
    assert_eq!(precision_at_k(&results, 2), 0.0);
}

// ---- distractor_at_k ----

#[test]
fn distractor_at_k_all_wrong() {
    let results: Vec<LabeledResult> = (0..4)
        .map(|i| make_result(i, RetrievalLabel::AdjacentWrong))
        .collect();
    assert!((distractor_at_k(&results, 4) - 1.0).abs() < 1e-9);
}

#[test]
fn distractor_at_k_none_wrong() {
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Irrelevant),
    ];
    assert_eq!(distractor_at_k(&results, 2), 0.0);
}

#[test]
fn distractor_at_k_mixed() {
    // 1 wrong in top-4 → 0.25
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::AdjacentWrong),
        make_result(2, RetrievalLabel::Irrelevant),
        make_result(3, RetrievalLabel::Background),
    ];
    assert!((distractor_at_k(&results, 4) - 0.25).abs() < 1e-9);
}

#[test]
fn distractor_at_k_zero_when_k_zero() {
    let results = vec![make_result(0, RetrievalLabel::AdjacentWrong)];
    assert_eq!(distractor_at_k(&results, 0), 0.0);
}

// ---- net_evidence_at_k ----

#[test]
fn net_evidence_at_k_single_decisive_rank1() {
    // Rank-1 Decisive: gain=3.0 / log2(2)=1.0 → 3.0
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    assert!((net_evidence_at_k(&results, 1) - 3.0).abs() < 1e-9);
}

#[test]
fn net_evidence_at_k_negative_for_all_wrong() {
    // Each AdjacentWrong at rank i contributes -2.0 / log2(i+2)
    let results: Vec<LabeledResult> = (0..3)
        .map(|i| make_result(i as u64, RetrievalLabel::AdjacentWrong))
        .collect();
    let score = net_evidence_at_k(&results, 3);
    assert!(
        score < 0.0,
        "all distractors should produce negative net evidence"
    );
}

#[test]
fn net_evidence_at_k_zero_for_empty() {
    assert_eq!(net_evidence_at_k(&[], 5), 0.0);
}

#[test]
fn net_evidence_at_k_zero_for_k_zero() {
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    assert_eq!(net_evidence_at_k(&results, 0), 0.0);
}

#[test]
fn net_evidence_at_k_mixed_sums_correctly() {
    // rank1=Decisive(3.0/log2(2)=3.0), rank2=Supporting(2.0/log2(3)≈1.2619)
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Supporting),
    ];
    let expected = 3.0 / 2.0_f64.log2() + 2.0 / 3.0_f64.log2();
    let actual = net_evidence_at_k(&results, 2);
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

// ---- ndcg_at_k ----

#[test]
fn ndcg_at_k_perfect_ranking() {
    // Perfect ranking: Decisive first → nDCG = 1.0
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Supporting),
        make_result(2, RetrievalLabel::Irrelevant),
    ];
    let score = ndcg_at_k(&results, 3);
    assert!(
        (score - 1.0).abs() < 1e-9,
        "perfect ranking should yield nDCG=1.0, got {score}"
    );
}

#[test]
fn ndcg_at_k_suboptimal_ranking() {
    // Irrelevant first, Decisive second → nDCG < 1.0
    let results = vec![
        make_result(0, RetrievalLabel::Irrelevant),
        make_result(1, RetrievalLabel::Decisive),
    ];
    let score = ndcg_at_k(&results, 2);
    assert!(
        score < 1.0 && score > 0.0,
        "suboptimal ranking should yield 0 < nDCG < 1.0, got {score}"
    );
}

#[test]
fn ndcg_at_k_all_irrelevant_vacuously_one() {
    // No positive-gain items → vacuously 1.0 (idcg = 0)
    let results = vec![
        make_result(0, RetrievalLabel::Irrelevant),
        make_result(1, RetrievalLabel::Irrelevant),
    ];
    let score = ndcg_at_k(&results, 2);
    assert!((score - 1.0).abs() < 1e-9);
}

#[test]
fn ndcg_at_k_zero_for_zero_k() {
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    assert_eq!(ndcg_at_k(&results, 0), 0.0);
}

#[test]
fn ndcg_at_k_clamped_not_above_one() {
    // Construct a case that could produce DCG > IDCG due to floating-point;
    // verify clamp keeps result ≤ 1.0.
    let results: Vec<LabeledResult> = (0..10)
        .map(|i| make_result(i, RetrievalLabel::Decisive))
        .collect();
    let score = ndcg_at_k(&results, 10);
    assert!(
        score <= 1.0 + 1e-12,
        "nDCG must not exceed 1.0, got {score}"
    );
}

// ---- mrr ----

#[test]
fn mrr_decisive_at_rank1() {
    let results = vec![
        make_result(0, RetrievalLabel::Decisive),
        make_result(1, RetrievalLabel::Irrelevant),
    ];
    assert!((mrr(&results) - 1.0).abs() < 1e-9);
}

#[test]
fn mrr_decisive_at_rank3() {
    let results = vec![
        make_result(0, RetrievalLabel::Irrelevant),
        make_result(1, RetrievalLabel::Supporting),
        make_result(2, RetrievalLabel::Decisive),
    ];
    // 1/3
    assert!((mrr(&results) - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn mrr_no_decisive() {
    let results = vec![
        make_result(0, RetrievalLabel::Supporting),
        make_result(1, RetrievalLabel::Irrelevant),
    ];
    assert_eq!(mrr(&results), 0.0);
}

#[test]
fn mrr_empty() {
    assert_eq!(mrr(&[]), 0.0);
}

// ---- compute_all ----

#[test]
fn compute_all_returns_correct_structure() {
    let results: Vec<LabeledResult> = (0..10)
        .map(|i| {
            let label = if i < 3 {
                RetrievalLabel::Decisive
            } else {
                RetrievalLabel::Irrelevant
            };
            make_result(i, label)
        })
        .collect();
    let metrics = compute_all(&results);

    // recall_at_k has 4 entries for k ∈ {1,3,5,10}
    assert_eq!(metrics.recall_at_k.len(), 4);
    assert_eq!(metrics.recall_at_k[0].0, 1);
    assert_eq!(metrics.recall_at_k[1].0, 3);
    assert_eq!(metrics.recall_at_k[2].0, 5);
    assert_eq!(metrics.recall_at_k[3].0, 10);

    // k=3: all 3 decisive in top-3 → recall=1.0
    assert!((metrics.recall_at_k[1].1 - 1.0).abs() < 1e-9);

    // MRR = 1.0 (decisive at rank 1)
    assert!((metrics.mrr - 1.0).abs() < 1e-9);

    // flip_ratio is None (no before/after pair provided)
    assert!(metrics.flip_ratio.is_none());
}

#[test]
fn compute_all_distractor_metric() {
    // 5 adjacent-wrong at ranks 1-5, rest irrelevant
    let results: Vec<LabeledResult> = (0..10)
        .map(|i| {
            let label = if i < 5 {
                RetrievalLabel::AdjacentWrong
            } else {
                RetrievalLabel::Irrelevant
            };
            make_result(i, label)
        })
        .collect();
    let metrics = compute_all(&results);
    // distractor_at_10 = 5/10 = 0.5
    assert!(
        (metrics.distractor_at_10 - 0.5).abs() < 1e-9,
        "got {}",
        metrics.distractor_at_10
    );
    // mrr = 0 (no Decisive)
    assert_eq!(metrics.mrr, 0.0);
}

// ---- serialization round-trip ----

#[test]
fn label_serde_roundtrip() {
    for label in [
        RetrievalLabel::Decisive,
        RetrievalLabel::Supporting,
        RetrievalLabel::Background,
        RetrievalLabel::Irrelevant,
        RetrievalLabel::AdjacentWrong,
    ] {
        let json = serde_json::to_string(&label).unwrap();
        let back: RetrievalLabel = serde_json::from_str(&json).unwrap();
        assert_eq!(label, back);
    }
}

#[test]
fn metrics_serde_roundtrip() {
    let results = vec![make_result(0, RetrievalLabel::Decisive)];
    let m = compute_all(&results);
    let json = serde_json::to_string(&m).unwrap();
    let back: RetrievalMetrics = serde_json::from_str(&json).unwrap();
    assert_eq!(back.recall_at_k.len(), 4);
    assert!((back.mrr - 1.0).abs() < 1e-9);
}
