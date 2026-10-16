//! A CJK-dominant query leaves English-only embedding models' vector lists out of fusion.
//!
//! "Weight zero" is pinned against the same candidates with that list emptied: the dropped list
//! keeps its source slot, so the result must equal fusing an empty list in that position.

use std::collections::HashSet;

use khive_fusion::FusionStrategy;
use khive_runtime::{SearchHit, SearchSource};
use khive_score::DeterministicScore;
use khive_storage::types::{TextSearchHit, VectorSearchHit};
use uuid::Uuid;

use super::common::*;
use crate::config::RecallConfig;

const ENGLISH_ONLY: &str = "all-minilm-l6-v2";
const MULTILINGUAL: &str = "paraphrase-multilingual-minilm-l12-v2";

const CJK_QUERY: &str = "重做嵌入索引之前必须做什么准备";
const ENGLISH_QUERY: &str = "what must be prepared before rebuilding the embedding index";
const MIXED_QUERY: &str = "KHIVE 的 recall 为什么返回空";

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn vector_hit(id: Uuid, raw: i64) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank: 1,
    }
}

fn text_hit(id: Uuid, raw: i64) -> TextSearchHit {
    TextSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank: 1,
        title: None,
        snippet: None,
    }
}

fn candidate_set(
    text_hits: Vec<TextSearchHit>,
    models: Vec<(&str, Vec<VectorSearchHit>)>,
) -> RecallCandidateSet {
    RecallCandidateSet {
        namespace: "local".to_owned(),
        text_hits,
        vector_hits_per_model: models
            .into_iter()
            .map(|(name, hits)| (name.to_owned(), hits))
            .collect(),
        visible_namespaces: vec!["local".to_owned()],
        ann_degraded: false,
        ann_degraded_reason: None,
        session_unmet_models: Vec::new(),
        timings: RecallStageTimings::default(),
    }
}

/// An English-only model reports inflated similarities for CJK text: its highest scores belong
/// to ids that are wrong, and id 4 is a candidate of this model alone.
fn english_only_list() -> Vec<VectorSearchHit> {
    vec![
        vector_hit(uid(3), 95),
        vector_hit(uid(4), 90),
        vector_hit(uid(1), 85),
        vector_hit(uid(2), 80),
    ]
}

fn multilingual_list() -> Vec<VectorSearchHit> {
    vec![
        vector_hit(uid(1), 70),
        vector_hit(uid(2), 60),
        vector_hit(uid(3), 50),
    ]
}

fn text_hits() -> Vec<TextSearchHit> {
    vec![text_hit(uid(5), 40), text_hit(uid(2), 30)]
}

/// The records 1 to 6 are the memories; id 9 is a hit that is not a memory.
fn memory_ids() -> HashSet<Uuid> {
    (1..=6).map(uid).collect()
}

fn both_models() -> RecallCandidateSet {
    candidate_set(
        text_hits(),
        vec![
            (ENGLISH_ONLY, english_only_list()),
            (MULTILINGUAL, multilingual_list()),
        ],
    )
}

/// Every strategy that takes per-model vector lists. The weighted-RRF weights differ per slot,
/// so a list that lost its slot would either mismatch the weight count or shift the weights.
fn vector_strategies(slots: usize) -> Vec<(&'static str, FusionStrategy)> {
    vec![
        ("rrf k=10", FusionStrategy::Rrf { k: 10 }),
        ("rrf k=60", FusionStrategy::Rrf { k: 60 }),
        (
            "weighted",
            FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
        ),
        ("union", FusionStrategy::Union),
        ("vector_only", FusionStrategy::VectorOnly),
        (
            "weighted_rrf",
            FusionStrategy::weighted_rrf(10, (0..slots).map(|i| (slots - i) as f64).collect()),
        ),
    ]
}

fn config(strategy: FusionStrategy) -> RecallConfig {
    RecallConfig {
        fuse_strategy: strategy,
        ..RecallConfig::default()
    }
}

fn rows(hits: &[SearchHit]) -> Vec<(Uuid, i64, SearchSource)> {
    hits.iter()
        .map(|hit| (hit.entity_id, hit.score.to_raw(), hit.source))
        .collect()
}

fn ids(hits: &[SearchHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.entity_id).collect()
}

#[test]
fn cjk_dominant_query_gives_the_english_only_list_weight_zero_under_every_vector_strategy() {
    let full = both_models();
    let emptied = candidate_set(
        text_hits(),
        vec![
            (ENGLISH_ONLY, Vec::new()),
            (MULTILINGUAL, multilingual_list()),
        ],
    );
    let memory = memory_ids();

    for (name, strategy) in vector_strategies(3) {
        let cfg = config(strategy);
        let kept = fuse_candidates(&full, &memory, &cfg, 10).unwrap();
        let reference = fuse_candidates(&emptied, &memory, &cfg, 10).unwrap();
        let dropped = fuse_for_query(&full, &memory, &cfg, 10, CJK_QUERY, None).unwrap();

        assert!(
            ids(&kept).contains(&uid(4)),
            "{name}: id 4 is in the English-only list alone, so keeping the list returns it"
        );
        assert!(
            !ids(&dropped).contains(&uid(4)),
            "{name}: a CJK-dominant query must not return an id only the English-only list has"
        );
        assert_eq!(
            rows(&dropped),
            rows(&reference),
            "{name}: dropping the list must equal fusing it empty"
        );
        assert_ne!(
            rows(&dropped),
            rows(&kept),
            "{name}: the English-only list must change the fusion when it is kept"
        );
    }
}

#[test]
fn english_only_list_is_dropped_wherever_it_sits_in_the_model_order() {
    let full = candidate_set(
        text_hits(),
        vec![
            (MULTILINGUAL, multilingual_list()),
            (ENGLISH_ONLY, english_only_list()),
        ],
    );
    let emptied = candidate_set(
        text_hits(),
        vec![
            (MULTILINGUAL, multilingual_list()),
            (ENGLISH_ONLY, Vec::new()),
        ],
    );
    let memory = memory_ids();

    for (name, strategy) in vector_strategies(3) {
        let cfg = config(strategy);
        let reference = fuse_candidates(&emptied, &memory, &cfg, 10).unwrap();
        let dropped = fuse_for_query(&full, &memory, &cfg, 10, CJK_QUERY, None).unwrap();

        assert_eq!(
            rows(&dropped),
            rows(&reference),
            "{name}: the English-only list is dropped from the second slot too"
        );
        assert!(!ids(&dropped).contains(&uid(4)), "{name}");
    }
}

#[test]
fn english_and_mixed_queries_fuse_exactly_as_before() {
    let full = both_models();
    let memory = memory_ids();
    let mut strategies = vector_strategies(3);
    strategies.push(("keyword_only", FusionStrategy::KeywordOnly));

    for query in [ENGLISH_QUERY, MIXED_QUERY, ""] {
        for (name, strategy) in &strategies {
            let cfg = config(strategy.clone());
            let before = fuse_candidates(&full, &memory, &cfg, 10).unwrap();
            let after = fuse_for_query(&full, &memory, &cfg, 10, query, None).unwrap();

            assert_eq!(
                rows(&after),
                rows(&before),
                "{name}: query {query:?} is not CJK-dominant, so its fusion must not change"
            );
            if *name != "keyword_only" {
                assert!(
                    ids(&after).contains(&uid(4)),
                    "{name}: query {query:?} keeps the English-only list in the fusion"
                );
            }
        }
    }
}

#[test]
fn explicit_embedding_model_request_keeps_every_list() {
    let full = both_models();
    let memory = memory_ids();

    for requested in [ENGLISH_ONLY, MULTILINGUAL] {
        for (name, strategy) in vector_strategies(3) {
            let cfg = config(strategy);
            let before = fuse_candidates(&full, &memory, &cfg, 10).unwrap();
            let after =
                fuse_for_query(&full, &memory, &cfg, 10, CJK_QUERY, Some(requested)).unwrap();

            assert_eq!(
                rows(&after),
                rows(&before),
                "{name}: embedding_model={requested} is an explicit request, never overridden"
            );
            assert!(ids(&after).contains(&uid(4)), "{name}: {requested}");
        }
    }
}

#[test]
fn english_only_list_stays_when_no_other_model_contributed_a_candidate() {
    let memory = memory_ids();
    let scenarios = [
        (
            "the English-only model is the only model",
            candidate_set(text_hits(), vec![(ENGLISH_ONLY, english_only_list())]),
        ),
        (
            "the other model returned no candidates",
            candidate_set(
                text_hits(),
                vec![
                    (ENGLISH_ONLY, english_only_list()),
                    (MULTILINGUAL, Vec::new()),
                ],
            ),
        ),
        (
            "the other model's only hit is not a memory",
            candidate_set(
                text_hits(),
                vec![
                    (ENGLISH_ONLY, english_only_list()),
                    (MULTILINGUAL, vec![vector_hit(uid(9), 70)]),
                ],
            ),
        ),
    ];

    for (scenario, candidates) in &scenarios {
        let slots = candidates.vector_hits_per_model.len() + 1;
        for (name, strategy) in vector_strategies(slots) {
            let cfg = config(strategy);
            let before = fuse_candidates(candidates, &memory, &cfg, 10).unwrap();
            let after = fuse_for_query(candidates, &memory, &cfg, 10, CJK_QUERY, None).unwrap();

            assert_eq!(
                rows(&after),
                rows(&before),
                "{name}: {scenario}, so the vector leg must not vanish"
            );
            assert!(ids(&after).contains(&uid(4)), "{name}: {scenario}");
        }
    }
}

#[test]
fn models_outside_the_english_only_list_are_never_dropped() {
    let candidates = candidate_set(
        text_hits(),
        vec![
            ("multilingual-e5-small", english_only_list()),
            (MULTILINGUAL, multilingual_list()),
        ],
    );
    let memory = memory_ids();

    for (name, strategy) in vector_strategies(3) {
        let cfg = config(strategy);
        let before = fuse_candidates(&candidates, &memory, &cfg, 10).unwrap();
        let after = fuse_for_query(&candidates, &memory, &cfg, 10, CJK_QUERY, None).unwrap();

        assert_eq!(rows(&after), rows(&before), "{name}");
        assert!(ids(&after).contains(&uid(4)), "{name}");
    }
}
