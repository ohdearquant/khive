//! The labelled fusion in `khive-retrieval` reproduces `rrf_fuse` once the caller applies the
//! exact-title bonus and the cut to the full fused order.

use khive_retrieval::hybrid::{combine_leg_first_appearance, fuse_labelled, HitLabel};
use khive_score::DeterministicScore;
use khive_storage::types::{TextSearchHit, VectorSearchHit};
use uuid::Uuid;

use super::{
    rrf_fuse, RankScoreKind, SearchHit, SearchSignals, SearchSource, EXACT_MATCH_BOOST, RRF_K,
};

const TITLES: [Option<&str>; 4] = [None, Some("Alpha"), Some("beta"), Some("")];
const QUERIES: [&str; 3] = ["alpha", "ALPHA", "zzz"];

/// Small deterministic generator so every run sees the same inputs.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: usize) -> usize {
        let stepped = self.0.wrapping_mul(6364136223846793005);
        self.0 = stepped.wrapping_add(1442695040888963407);
        ((self.0 >> 33) % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

fn text_hit(id: u128, raw: i64, title: Option<&str>, snippet: Option<&str>) -> TextSearchHit {
    TextSearchHit {
        subject_id: Uuid::from_u128(id),
        score: DeterministicScore::from_raw(raw),
        rank: 0,
        title: title.map(str::to_string),
        snippet: snippet.map(str::to_string),
    }
}

fn vector_hit(id: u128, raw: i64) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: Uuid::from_u128(id),
        score: DeterministicScore::from_raw(raw),
        rank: 0,
    }
}

/// Small id pool, so ids repeat inside a leg and tie across legs.
fn random_inputs(rng: &mut Lcg) -> (Vec<TextSearchHit>, Vec<VectorSearchHit>) {
    let mut text = Vec::new();
    for _ in 0..rng.below(7) {
        let id = 1 + rng.below(5) as u128;
        let raw = rng.below(1000) as i64;
        let title = *rng.pick(&TITLES);
        let snippet = *rng.pick(&TITLES);
        text.push(text_hit(id, raw, title, snippet));
    }
    let mut vector = Vec::new();
    for _ in 0..rng.below(7) {
        let id = 1 + rng.below(5) as u128;
        let raw = rng.below(1000) as i64;
        vector.push(vector_hit(id, raw));
    }
    (text, vector)
}

/// What a caller of `fuse_labelled` does in place of `rrf_fuse`: build one labelled arm per leg,
/// fuse, add the exact-title bonus to the full fused order, then sort and cut.
fn labelled_rrf_fuse(
    text: &[TextSearchHit],
    vector: &[VectorSearchHit],
    limit: usize,
    query_text: &str,
) -> Vec<SearchHit> {
    let mut text_arm = Vec::new();
    for (rank, hit) in text.iter().enumerate() {
        let signals = SearchSignals {
            vector_similarity: None,
            keyword_score: Some(hit.score),
        };
        let label = HitLabel {
            rank,
            signals,
            source: SearchSource::Text,
            title: hit.title.clone(),
            snippet: hit.snippet.clone(),
        };
        text_arm.push((hit.subject_id, label));
    }
    let mut vector_arm = Vec::new();
    for (rank, hit) in vector.iter().enumerate() {
        let signals = SearchSignals {
            vector_similarity: Some(hit.score),
            keyword_score: None,
        };
        let label = HitLabel {
            rank,
            signals,
            source: SearchSource::Vector,
            title: None,
            snippet: None,
        };
        vector_arm.push((hit.subject_id, label));
    }

    let fused = fuse_labelled(
        vec![text_arm, vector_arm],
        RRF_K,
        combine_leg_first_appearance,
    );

    let query_lower = query_text.to_lowercase();
    let boost = DeterministicScore::from_f64(EXACT_MATCH_BOOST);
    let mut hits = Vec::new();
    for (entity_id, score, label) in fused {
        let title_lower = label.title.as_deref().map(str::to_lowercase);
        let exact = title_lower.as_deref() == Some(query_lower.as_str());
        let score = if exact { score + boost } else { score };
        hits.push(SearchHit {
            entity_id,
            score,
            rank_score_kind: RankScoreKind::Rrf,
            signals: label.signals,
            source: label.source,
            title: label.title,
            snippet: label.snippet,
        });
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
}

#[test]
fn labelled_fusion_matches_rrf_fuse_over_random_inputs() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..400 {
        let (text, vector) = random_inputs(&mut rng);
        let query = *rng.pick(&QUERIES);
        let limit = 1 + rng.below(8);

        let expected = rrf_fuse(text.clone(), vector.clone(), limit, query);
        let actual = labelled_rrf_fuse(&text, &vector, limit, query);

        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }
}

#[test]
fn labelled_fusion_matches_rrf_fuse_for_a_repeated_text_id_with_a_late_title() {
    // The first text copy of an id decides its labels. Later copies are ignored even when the
    // first one carries none, and the exact-title bonus follows the title that was kept.
    let text = vec![
        text_hit(1, 5, None, None),
        text_hit(1, 4, Some("alpha"), Some("late snippet")),
        text_hit(2, 3, Some("alpha"), None),
    ];
    let vector = vec![vector_hit(1, 9)];

    let expected = rrf_fuse(text.clone(), vector.clone(), 10, "alpha");
    let actual = labelled_rrf_fuse(&text, &vector, 10, "alpha");

    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    assert_eq!(
        actual[0].entity_id,
        Uuid::from_u128(2),
        "the exact title outranks the repeated id"
    );
    assert_eq!(actual[1].title, None);
    assert_eq!(actual[1].snippet, None);
}

#[test]
fn labelled_fusion_applies_the_title_bonus_before_the_cut() {
    let text = vec![
        text_hit(1, 5, None, None),
        text_hit(2, 4, None, None),
        text_hit(3, 3, Some("Alpha"), None),
    ];

    let expected = rrf_fuse(text.clone(), Vec::new(), 1, "alpha");
    let actual = labelled_rrf_fuse(&text, &[], 1, "alpha");

    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    assert_eq!(actual[0].entity_id, Uuid::from_u128(3));
}
