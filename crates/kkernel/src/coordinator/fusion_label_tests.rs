//! The labelled fusion in `khive-retrieval` reproduces the cross-backend merges once the caller
//! cuts the full fused order itself.

use khive_retrieval::hybrid::{combine_best_ranked_evidence, fuse_labelled, HitLabel};
use khive_runtime::{NoteSearchHit, RankScoreKind, SearchHit, SearchSignals, SearchSource};
use khive_score::DeterministicScore;
use uuid::Uuid;

use super::dispatch::{rrf_merge_entity_hits, rrf_merge_note_hits};

type Fused = Vec<(Uuid, DeterministicScore, HitLabel)>;

const TITLES: [Option<&str>; 4] = [None, Some("alpha"), Some("beta"), Some("")];

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

/// One appearance of an id in one backend's ranked list.
struct Appearance {
    id: Uuid,
    signals: SearchSignals,
    source: SearchSource,
    title: Option<String>,
    snippet: Option<String>,
}

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

fn appearance(
    id: Uuid,
    signals: SearchSignals,
    source: SearchSource,
    title: Option<&str>,
) -> Appearance {
    Appearance {
        id,
        signals,
        source,
        title: title.map(str::to_string),
        snippet: title.map(|t| format!("{t} snippet")),
    }
}

/// Small id pool, so ids repeat inside a list and tie across lists.
fn random_appearance(rng: &mut Lcg) -> Appearance {
    let raw = rng.below(1000) as i64;
    let signals = match rng.below(3) {
        0 => keyword(raw),
        1 => vector(raw),
        _ => SearchSignals::default(),
    };
    let source = match rng.below(3) {
        0 => SearchSource::Text,
        1 => SearchSource::Vector,
        _ => SearchSource::Both,
    };
    let title = *rng.pick(&TITLES);
    let snippet = *rng.pick(&TITLES);
    Appearance {
        id: Uuid::from_u128(1 + rng.below(5) as u128),
        signals,
        source,
        title: title.map(str::to_string),
        snippet: snippet.map(str::to_string),
    }
}

fn random_lists(rng: &mut Lcg) -> Vec<Vec<Appearance>> {
    let mut lists = Vec::new();
    for _ in 0..1 + rng.below(3) {
        let mut list = Vec::new();
        for _ in 0..rng.below(7) {
            list.push(random_appearance(rng));
        }
        lists.push(list);
    }
    lists
}

fn entity_hit(a: &Appearance) -> SearchHit {
    SearchHit {
        entity_id: a.id,
        score: DeterministicScore::ZERO,
        rank_score_kind: RankScoreKind::Rrf,
        signals: a.signals,
        source: a.source,
        title: a.title.clone(),
        snippet: a.snippet.clone(),
    }
}

fn note_hit(a: &Appearance) -> NoteSearchHit {
    NoteSearchHit {
        note_id: a.id,
        score: DeterministicScore::ZERO,
        rank_score_kind: RankScoreKind::Rrf,
        signals: a.signals,
        source: a.source,
        title: a.title.clone(),
        snippet: a.snippet.clone(),
    }
}

fn map_lists<T>(lists: &[Vec<Appearance>], make: fn(&Appearance) -> T) -> Vec<Vec<T>> {
    let mut mapped = Vec::new();
    for list in lists {
        mapped.push(list.iter().map(make).collect());
    }
    mapped
}

/// What a caller of `fuse_labelled` does in place of the merges: one labelled arm per list,
/// fuse, then cut the full fused order.
fn labelled_merge(lists: &[Vec<Appearance>], limit: usize) -> Fused {
    let mut arms = Vec::new();
    for list in lists {
        let mut arm = Vec::new();
        for (rank, a) in list.iter().enumerate() {
            let label = HitLabel {
                rank,
                signals: a.signals,
                source: a.source,
                title: a.title.clone(),
                snippet: a.snippet.clone(),
            };
            arm.push((a.id, label));
        }
        arms.push(arm);
    }
    let mut fused = fuse_labelled(arms, 60, combine_best_ranked_evidence);
    fused.truncate(limit);
    fused
}

fn into_entity_hits(fused: Fused) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for (entity_id, score, label) in fused {
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
    hits
}

fn into_note_hits(fused: Fused) -> Vec<NoteSearchHit> {
    let mut hits = Vec::new();
    for (note_id, score, label) in fused {
        hits.push(NoteSearchHit {
            note_id,
            score,
            rank_score_kind: RankScoreKind::Rrf,
            signals: label.signals,
            source: label.source,
            title: label.title,
            snippet: label.snippet,
        });
    }
    hits
}

#[test]
fn labelled_fusion_matches_the_entity_and_note_merges_over_random_lists() {
    let mut rng = Lcg(0xfeed);
    for _ in 0..400 {
        let lists = random_lists(&mut rng);
        let limit = 1 + rng.below(8);

        let expected = rrf_merge_entity_hits(map_lists(&lists, entity_hit), limit);
        let actual = into_entity_hits(labelled_merge(&lists, limit));
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));

        let expected = rrf_merge_note_hits(map_lists(&lists, note_hit), limit);
        let actual = into_note_hits(labelled_merge(&lists, limit));
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }
}

#[test]
fn labelled_fusion_matches_the_merges_for_tied_evidence_and_late_labels() {
    // `x` is first seen at rank 0 without a title, again later in the same list with a title and
    // snippet, and at rank 0 in the second list with other signals: a tie the earlier list wins.
    let x = Uuid::from_u128(1);
    let y = Uuid::from_u128(2);
    let one = vec![
        appearance(x, vector(1), SearchSource::Vector, None),
        appearance(y, keyword(7), SearchSource::Text, None),
        appearance(x, keyword(9), SearchSource::Text, Some("late")),
    ];
    let other = appearance(x, keyword(2), SearchSource::Text, Some("other"));
    let lists = [one, vec![other]];

    let expected = rrf_merge_entity_hits(map_lists(&lists, entity_hit), 10);
    let actual = into_entity_hits(labelled_merge(&lists, 10));

    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    let merged = &actual[0];
    assert_eq!(merged.entity_id, x);
    assert_eq!(merged.signals, vector(1));
    assert_eq!(merged.source, SearchSource::Both);
    assert_eq!(merged.title.as_deref(), Some("late"));
    assert_eq!(merged.snippet.as_deref(), Some("late snippet"));
}
