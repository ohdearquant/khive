//! Pins `fuse_candidates` to the body it had before its labels were rebuilt through the shared
//! labelled fusion: the same candidate sets give the same ids, order, scores, sources, titles and
//! snippets under every fusion strategy.

use std::collections::{HashMap, HashSet};

use khive_fusion::FusionStrategy;
use khive_retrieval::fuse_search_results;
use khive_runtime::{RankScoreKind, SearchHit, SearchSignals, SearchSource};
use khive_score::DeterministicScore;
use khive_storage::types::{TextSearchHit, VectorSearchHit};
use uuid::Uuid;

use super::common::*;
use crate::config::RecallConfig;

/// What the previous body kept for each id while it collected the arms.
#[derive(Default)]
struct ReferenceMeta {
    in_text: bool,
    in_vector: bool,
    title: Option<String>,
    snippet: Option<String>,
}

fn reference_source(meta: &ReferenceMeta) -> SearchSource {
    match (meta.in_vector, meta.in_text) {
        (true, true) => SearchSource::Both,
        (true, false) => SearchSource::Vector,
        (false, true) => SearchSource::Text,
        (false, false) => SearchSource::Text,
    }
}

/// The body `fuse_candidates` had before its labels were rebuilt through the shared labelled
/// fusion, with the helper types renamed and nothing else changed.
fn reference_fuse_candidates(
    candidates: &RecallCandidateSet,
    memory_ids: &HashSet<Uuid>,
    cfg: &RecallConfig,
    limit: usize,
) -> Vec<SearchHit> {
    let mut meta = HashMap::<Uuid, ReferenceMeta>::new();

    let text_source: Vec<_> = candidates
        .text_hits
        .iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .map(|h| {
            let entry = meta.entry(h.subject_id).or_default();
            entry.in_text = true;
            if entry.title.is_none() {
                entry.title = h.title.clone();
            }
            if entry.snippet.is_none() {
                entry.snippet = h.snippet.clone();
            }
            (h.subject_id, h.score)
        })
        .collect();

    let vector_sources: Vec<Vec<_>> = candidates
        .vector_hits_per_model
        .iter()
        .map(|(_, hits)| {
            hits.iter()
                .filter(|h| memory_ids.contains(&h.subject_id))
                .map(|h| {
                    meta.entry(h.subject_id).or_default().in_vector = true;
                    (h.subject_id, h.score)
                })
                .collect()
        })
        .collect();

    let vector_only = matches!(&cfg.fuse_strategy, FusionStrategy::VectorOnly);
    let keyword_only = matches!(&cfg.fuse_strategy, FusionStrategy::KeywordOnly);
    let is_weighted = matches!(&cfg.fuse_strategy, FusionStrategy::Weighted { .. });

    let sources: Vec<Vec<_>> = if vector_only {
        vec![combine_vector_sources_union(vector_sources), vec![]]
    } else if keyword_only {
        vec![vec![], text_source]
    } else if is_weighted && vector_sources.len() > 1 {
        let combined_vector = combine_vector_sources_union(vector_sources);
        vec![combined_vector, text_source]
    } else {
        let mut s = if vector_sources.is_empty() {
            vec![vec![]]
        } else {
            vector_sources
        };
        s.push(text_source);
        s
    };

    if sources.is_empty() || sources.iter().all(|s| s.is_empty()) {
        return vec![];
    }

    let retrieval_cfg = retrieval_hybrid_config(&cfg.fuse_strategy, limit);
    fuse_search_results(sources, &retrieval_cfg)
        .into_iter()
        .map(|(id, score)| {
            let m = meta.remove(&id).unwrap_or_default();
            let (source, title, snippet) = if vector_only {
                (SearchSource::Vector, None, None)
            } else if keyword_only {
                (SearchSource::Text, m.title, m.snippet)
            } else {
                (reference_source(&m), m.title, m.snippet)
            };
            SearchHit {
                entity_id: id,
                score,
                rank_score_kind: match &cfg.fuse_strategy {
                    FusionStrategy::Rrf { .. } => RankScoreKind::Rrf,
                    FusionStrategy::VectorOnly => RankScoreKind::Vector,
                    FusionStrategy::KeywordOnly => RankScoreKind::Keyword,
                    FusionStrategy::Weighted { .. } => RankScoreKind::Weighted,
                    FusionStrategy::Union => RankScoreKind::Union,
                    FusionStrategy::Custom { .. } => RankScoreKind::Rrf,
                },
                signals: SearchSignals::default(),
                source,
                title,
                snippet,
            }
        })
        .collect()
}

/// Every field of a hit that the equivalence compares, with the score as its raw fixed-point bits.
#[derive(Debug, PartialEq)]
struct Row {
    id: Uuid,
    raw_score: i64,
    kind: RankScoreKind,
    signals: SearchSignals,
    source: SearchSource,
    title: Option<String>,
    snippet: Option<String>,
}

fn rows(hits: &[SearchHit]) -> Vec<Row> {
    hits.iter()
        .map(|hit| Row {
            id: hit.entity_id,
            raw_score: hit.score.to_raw(),
            kind: hit.rank_score_kind,
            signals: hit.signals,
            source: hit.source,
            title: hit.title.clone(),
            snippet: hit.snippet.clone(),
        })
        .collect()
}

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn text_hit(id: Uuid, raw: i64, title: Option<&str>, snippet: Option<&str>) -> TextSearchHit {
    TextSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank: 1,
        title: title.map(str::to_owned),
        snippet: snippet.map(str::to_owned),
    }
}

fn vector_hit(id: Uuid, raw: i64) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: id,
        score: DeterministicScore::from_raw(raw),
        rank: 1,
    }
}

fn candidate_set(
    text_hits: Vec<TextSearchHit>,
    models: Vec<Vec<VectorSearchHit>>,
) -> RecallCandidateSet {
    RecallCandidateSet {
        namespace: "local".to_owned(),
        text_hits,
        vector_hits_per_model: models
            .into_iter()
            .enumerate()
            .map(|(index, hits)| (format!("model-{index}"), hits))
            .collect(),
        visible_namespaces: vec!["local".to_owned()],
        ann_degraded: false,
        ann_degraded_reason: None,
        session_unmet_models: Vec::new(),
        timings: RecallStageTimings::default(),
    }
}

/// The record ids 1 to 5 are the memories; id 9 is a hit that is not a memory.
fn memory_ids() -> HashSet<Uuid> {
    (1..=5).map(uid).collect()
}

fn strategies() -> Vec<FusionStrategy> {
    let custom = FusionStrategy::try_custom("custom".to_owned(), serde_json::json!({}))
        .expect("a non-empty name is a valid custom strategy");
    vec![
        FusionStrategy::Rrf { k: 60 },
        FusionStrategy::Rrf { k: 1 },
        FusionStrategy::VectorOnly,
        FusionStrategy::KeywordOnly,
        FusionStrategy::Weighted {
            weights: vec![0.7, 0.3],
        },
        FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
        FusionStrategy::Union,
        custom,
    ]
}

/// Candidate sets with ties, repeated ids, hits that are not memories, an empty list and lists
/// with one leg only.
fn scenarios() -> Vec<(&'static str, RecallCandidateSet)> {
    let (one, two, three, four, five) = (uid(1), uid(2), uid(3), uid(4), uid(5));
    let hidden = uid(9);

    let nothing = candidate_set(Vec::new(), Vec::new());
    let empty_models = candidate_set(Vec::new(), vec![Vec::new(), Vec::new()]);

    let text_only = candidate_set(
        vec![
            text_hit(one, 90, Some("one"), Some("one snippet")),
            text_hit(two, 80, None, Some("two snippet")),
            text_hit(three, 70, Some("three"), None),
        ],
        Vec::new(),
    );

    let lone_model = vec![
        vector_hit(three, 80),
        vector_hit(one, 60),
        vector_hit(two, 40),
        vector_hit(four, 20),
    ];
    let vector_only = candidate_set(Vec::new(), vec![lone_model]);

    let tied_text = vec![
        text_hit(three, 50, Some("three"), None),
        text_hit(one, 50, Some("one"), None),
        text_hit(two, 50, Some("two"), None),
    ];
    let tied_a = vec![
        vector_hit(two, 70),
        vector_hit(three, 70),
        vector_hit(one, 70),
    ];
    let tied_b = vec![
        vector_hit(one, 70),
        vector_hit(two, 70),
        vector_hit(three, 70),
    ];
    let ties = candidate_set(tied_text, vec![tied_a, tied_b]);

    let repeated_text = vec![
        text_hit(one, 90, None, None),
        text_hit(two, 80, Some("two"), Some("two snippet")),
        text_hit(one, 70, Some("one late"), Some("one late snippet")),
        text_hit(one, 60, Some("one later"), None),
    ];
    let repeated_a = vec![
        vector_hit(two, 95),
        vector_hit(two, 90),
        vector_hit(three, 85),
    ];
    let repeated_b = vec![
        vector_hit(three, 95),
        vector_hit(three, 80),
        vector_hit(one, 10),
    ];
    let repeated = candidate_set(repeated_text, vec![repeated_a, repeated_b]);

    let overlap_text = vec![
        text_hit(two, 90, Some("two"), Some("two snippet")),
        text_hit(one, 80, Some("one"), None),
        text_hit(four, 70, Some("four"), Some("four snippet")),
    ];
    let overlap_a = vec![
        vector_hit(one, 90),
        vector_hit(three, 80),
        vector_hit(two, 70),
    ];
    let overlap_b = vec![vector_hit(two, 90), vector_hit(five, 20)];
    let overlap = candidate_set(overlap_text, vec![overlap_a, overlap_b]);

    let foreign_text = vec![
        text_hit(hidden, 99, Some("hidden"), None),
        text_hit(one, 50, Some("one"), None),
    ];
    let foreign_a = vec![vector_hit(hidden, 98), vector_hit(two, 40)];
    let foreign = candidate_set(foreign_text, vec![foreign_a]);

    let disjoint_text = vec![
        text_hit(four, 90, Some("four"), None),
        text_hit(five, 80, Some("five"), Some("five snippet")),
    ];
    let disjoint_a = vec![vector_hit(one, 90), vector_hit(two, 80)];
    let disjoint = candidate_set(disjoint_text, vec![disjoint_a]);

    vec![
        ("no hits at all", nothing),
        ("models without hits", empty_models),
        ("text hits only", text_only),
        ("one vector model only", vector_only),
        ("equal scores", ties),
        ("repeated ids", repeated),
        ("both legs return the same ids", overlap),
        ("hits that are not memories", foreign),
        ("disjoint legs", disjoint),
    ]
}

#[test]
fn fuse_candidates_matches_the_previous_body_for_every_strategy() {
    let memory_ids = memory_ids();
    let mut saw_both = false;
    let mut saw_repeated_id = false;
    let mut saw_empty = false;

    for (name, candidates) in scenarios() {
        for strategy in strategies() {
            for limit in [10, 3, 1] {
                let cfg = RecallConfig {
                    fuse_strategy: strategy.clone(),
                    ..RecallConfig::default()
                };
                let got = fuse_candidates(&candidates, &memory_ids, &cfg, limit);
                let want = reference_fuse_candidates(&candidates, &memory_ids, &cfg, limit);
                let got_rows = rows(&got);
                assert_eq!(
                    got_rows,
                    rows(&want),
                    "{name} under {strategy:?} at limit {limit}"
                );

                for row in &got_rows {
                    saw_both |= row.source == SearchSource::Both;
                }
                let distinct: HashSet<Uuid> = got.iter().map(|hit| hit.entity_id).collect();
                saw_repeated_id |= distinct.len() < got.len();
                saw_empty |= got.is_empty();
            }
        }
    }

    assert!(saw_both, "a scenario must return a hit from both legs");
    assert!(saw_repeated_id, "a scenario must return a repeated id");
    assert!(saw_empty, "a scenario must return no hits");
}

#[test]
fn keyword_only_repeated_id_keeps_its_labels_on_the_first_copy_only() {
    let first = uid(1);
    let second = uid(2);
    let candidates = candidate_set(
        vec![
            text_hit(first, 90, None, Some("first snippet")),
            text_hit(first, 80, Some("late title"), Some("late snippet")),
            text_hit(second, 70, Some("second"), Some("second snippet")),
        ],
        Vec::new(),
    );
    let memory_ids = HashSet::from([first, second]);
    let cfg = RecallConfig {
        fuse_strategy: FusionStrategy::KeywordOnly,
        ..RecallConfig::default()
    };

    let hits = fuse_candidates(&candidates, &memory_ids, &cfg, 10);

    assert_eq!(
        hits.len(),
        3,
        "a pass-through strategy returns one hit per copy"
    );
    assert_eq!(hits[0].entity_id, first);
    assert_eq!(hits[0].score.to_raw(), 90);
    assert!(matches!(hits[0].source, SearchSource::Text));
    assert_eq!(hits[0].title.as_deref(), Some("late title"));
    assert_eq!(hits[0].snippet.as_deref(), Some("first snippet"));
    assert_eq!(hits[1].entity_id, first);
    assert_eq!(hits[1].score.to_raw(), 80);
    assert!(matches!(hits[1].source, SearchSource::Text));
    assert_eq!(hits[1].title, None);
    assert_eq!(hits[1].snippet, None);
    assert_eq!(hits[2].entity_id, second);
    assert_eq!(hits[2].title.as_deref(), Some("second"));
    assert_eq!(hits[2].snippet.as_deref(), Some("second snippet"));
}
