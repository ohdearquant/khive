//! `rrf_fuse` fuses entity hits through the shared labelled fusion in `khive-retrieval`. These
//! tests hold it to the hand-rolled algorithm it replaced, kept below as `reference_rrf_fuse`:
//! the same ids in the same order, with the same scores, signals, sources, titles and snippets.

use std::collections::{HashMap, HashSet};

use khive_score::{rrf_score, DeterministicScore};
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

/// The hand-rolled fusion `rrf_fuse` used before it moved onto the shared labelled fusion, kept
/// unchanged as the reference the ported path is held to.
fn reference_rrf_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    limit: usize,
    query_text: &str,
) -> Vec<SearchHit> {
    #[derive(Default)]
    struct Bucket {
        score: DeterministicScore,
        signals: SearchSignals,
        source: Option<SearchSource>,
        title: Option<String>,
        snippet: Option<String>,
    }

    let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();

    let query_lower = query_text.to_lowercase();
    let mut text_seen = HashSet::with_capacity(text_hits.len());
    for (i, hit) in text_hits.into_iter().enumerate() {
        if !text_seen.insert(hit.subject_id) {
            continue;
        }
        let rank = i + 1; // RRF is 1-indexed
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(rank, RRF_K);
        entry.signals.keyword_score = Some(hit.score);
        entry.source = Some(match entry.source {
            Some(SearchSource::Vector) => SearchSource::Both,
            _ => SearchSource::Text,
        });
        if entry.title.is_none() {
            // Apply exact-match boost before storing the title so we only check once.
            if let Some(ref title) = hit.title {
                if title.to_lowercase() == query_lower {
                    entry.score = entry.score + DeterministicScore::from_f64(EXACT_MATCH_BOOST);
                }
            }
            entry.title = hit.title;
        }
        if entry.snippet.is_none() {
            entry.snippet = hit.snippet;
        }
    }

    let mut vector_seen = HashSet::with_capacity(vector_hits.len());
    for (i, hit) in vector_hits.into_iter().enumerate() {
        if !vector_seen.insert(hit.subject_id) {
            continue;
        }
        let rank = i + 1;
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(rank, RRF_K);
        entry.signals.vector_similarity = Some(hit.score);
        entry.source = Some(match entry.source {
            Some(SearchSource::Text) => SearchSource::Both,
            _ => SearchSource::Vector,
        });
    }

    let mut hits: Vec<SearchHit> = buckets
        .into_iter()
        .map(|(id, b)| SearchHit {
            entity_id: id,
            score: b.score,
            rank_score_kind: RankScoreKind::Rrf,
            signals: b.signals,
            source: b.source.expect("each bucket gets a source"),
            title: b.title,
            snippet: b.snippet,
        })
        .collect();

    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
}

/// Run `rrf_fuse` and the reference on the same inputs and require every field of every hit to
/// agree, scores to the last bit. Returns the ported result for further checks.
fn fuse_and_compare(
    text: &[TextSearchHit],
    vector: &[VectorSearchHit],
    limit: usize,
    query: &str,
) -> Vec<SearchHit> {
    let actual = rrf_fuse(text.to_vec(), vector.to_vec(), limit, query);
    let expected = reference_rrf_fuse(text.to_vec(), vector.to_vec(), limit, query);

    assert_eq!(actual.len(), expected.len());
    for (got, want) in actual.iter().zip(&expected) {
        assert_eq!(got.entity_id, want.entity_id);
        assert_eq!(got.score, want.score);
        assert_eq!(got.rank_score_kind, want.rank_score_kind);
        assert_eq!(got.signals, want.signals);
        assert_eq!(got.source, want.source);
        assert_eq!(got.title, want.title);
        assert_eq!(got.snippet, want.snippet);
    }
    actual
}

#[test]
fn rrf_fuse_matches_the_reference_over_random_inputs() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..400 {
        let (text, vector) = random_inputs(&mut rng);
        let query = *rng.pick(&QUERIES);
        let limit = 1 + rng.below(8);

        fuse_and_compare(&text, &vector, limit, query);
    }
}

#[test]
fn rrf_fuse_matches_the_reference_for_a_repeated_text_id_with_a_late_title() {
    // The first text copy of an id decides its labels. Later copies are ignored even when the
    // first one carries none, and the exact-title bonus follows the title that was kept.
    let text = [
        text_hit(1, 5, None, None),
        text_hit(1, 4, Some("alpha"), Some("late snippet")),
        text_hit(2, 3, Some("alpha"), None),
    ];
    let vector = [vector_hit(1, 9)];

    let actual = fuse_and_compare(&text, &vector, 10, "alpha");

    assert_eq!(
        actual[0].entity_id,
        Uuid::from_u128(2),
        "the exact title outranks the repeated id"
    );
    assert_eq!(actual[1].title, None);
    assert_eq!(actual[1].snippet, None);
}

#[test]
fn rrf_fuse_matches_the_reference_when_the_bonus_decides_the_cut() {
    let text = [
        text_hit(1, 5, None, None),
        text_hit(2, 4, None, None),
        text_hit(3, 3, Some("Alpha"), None),
    ];

    let actual = fuse_and_compare(&text, &[], 1, "alpha");

    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].entity_id, Uuid::from_u128(3));
}

#[test]
fn rrf_fuse_matches_the_reference_for_tied_scores() {
    // Id 2 leads the text leg and id 1 leads the vector leg, so both score the same single
    // vote. Equal scores order by ascending id, whichever leg carried the id.
    let text = [text_hit(2, 5, None, None), text_hit(3, 4, None, None)];
    let vector = [vector_hit(1, 9), vector_hit(3, 8)];

    let actual = fuse_and_compare(&text, &vector, 10, "query");

    let ids: Vec<Uuid> = actual.iter().map(|hit| hit.entity_id).collect();
    let expected: Vec<Uuid> = [3, 1, 2].into_iter().map(Uuid::from_u128).collect();
    assert_eq!(ids, expected);
    assert_eq!(actual[1].score, actual[2].score);
}

#[test]
fn rrf_fuse_matches_the_reference_for_empty_and_one_sided_lists() {
    let text = [
        text_hit(1, 5, Some("Alpha"), Some("snippet")),
        text_hit(2, 4, None, None),
    ];
    let vector = [vector_hit(1, 9), vector_hit(2, 8)];

    let neither = fuse_and_compare(&[], &[], 10, "alpha");
    let text_only = fuse_and_compare(&text, &[], 10, "alpha");
    let vector_only = fuse_and_compare(&[], &vector, 10, "alpha");

    assert!(neither.is_empty());

    let sources: Vec<SearchSource> = text_only.iter().map(|hit| hit.source).collect();
    assert_eq!(sources, [SearchSource::Text; 2]);
    assert_eq!(text_only[0].title.as_deref(), Some("Alpha"));

    let sources: Vec<SearchSource> = vector_only.iter().map(|hit| hit.source).collect();
    assert_eq!(sources, [SearchSource::Vector; 2]);
    assert!(vector_only[0].title.is_none());
}
