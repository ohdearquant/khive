//! The cross-backend merges run the shared labelled fusion. The hand-rolled merge they replaced is
//! kept here as a reference copy, and the merges must match it id for id, score for score and
//! label for label, under every source filter.

use std::collections::{HashMap, HashSet};

use khive_runtime::{NoteSearchHit, RankScoreKind, SearchHit, SearchSignals, SearchSource};
use khive_score::{rrf_score, DeterministicScore};
use uuid::Uuid;

use super::dispatch::{rrf_merge_entity_hits_filtered, rrf_merge_note_hits_filtered};

/// One merged hit, whichever hit type carries it.
type Row = (
    Uuid,
    DeterministicScore,
    RankScoreKind,
    SearchSignals,
    SearchSource,
    Option<String>,
    Option<String>,
);

const TITLES: [Option<&str>; 4] = [None, Some("alpha"), Some("beta"), Some("")];

const FILTERS: [Option<SearchSource>; 4] = [
    None,
    Some(SearchSource::Text),
    Some(SearchSource::Vector),
    Some(SearchSource::Both),
];

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
#[derive(Clone)]
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

fn entity_rows(hits: Vec<SearchHit>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(hits.len());
    for hit in hits {
        rows.push((
            hit.entity_id,
            hit.score,
            hit.rank_score_kind,
            hit.signals,
            hit.source,
            hit.title,
            hit.snippet,
        ));
    }
    rows
}

fn note_rows(hits: Vec<NoteSearchHit>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(hits.len());
    for hit in hits {
        rows.push((
            hit.note_id,
            hit.score,
            hit.rank_score_kind,
            hit.signals,
            hit.source,
            hit.title,
            hit.snippet,
        ));
    }
    rows
}

fn ids_of(rows: &[Row]) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(row.0);
    }
    ids
}

fn entity_merge(
    lists: &[Vec<Appearance>],
    limit: usize,
    source_filter: Option<SearchSource>,
) -> Vec<Row> {
    let hits = rrf_merge_entity_hits_filtered(map_lists(lists, entity_hit), limit, source_filter);
    entity_rows(hits)
}

fn note_merge(
    lists: &[Vec<Appearance>],
    limit: usize,
    source_filter: Option<SearchSource>,
) -> Vec<Row> {
    let hits = rrf_merge_note_hits_filtered(map_lists(lists, note_hit), limit, source_filter);
    note_rows(hits)
}

/// What the previous merge kept for one id.
#[derive(Default)]
struct ReferenceBucket {
    score: DeterministicScore,
    // Ranked lists follow backend ID order; equal ranks retain the earlier backend.
    evidence_rank: Option<usize>,
    signals: SearchSignals,
    source: Option<SearchSource>,
    title: Option<String>,
    snippet: Option<String>,
}

/// The merge as it stood before it moved onto the shared fusion, kept verbatim as the oracle.
fn reference_merge(
    lists: &[Vec<Appearance>],
    limit: usize,
    source_filter: Option<SearchSource>,
) -> Vec<Row> {
    const K: usize = 60;

    let mut scores: HashMap<Uuid, ReferenceBucket> = HashMap::new();

    for list in lists {
        // A list votes once per id: a repeated id scores only at the position of its
        // first occurrence. Its later copies still contribute source, title and snippet.
        let mut seen = HashSet::with_capacity(list.len());
        for (i, hit) in list.iter().enumerate() {
            let entry = scores.entry(hit.id).or_default();
            if seen.insert(hit.id) {
                entry.score = entry.score + rrf_score(i + 1, K);
            }
            if entry.evidence_rank.is_none_or(|best_rank| i < best_rank) {
                entry.evidence_rank = Some(i);
                entry.signals = hit.signals;
            }
            entry.source = Some(match entry.source {
                Some(source) => source.union(hit.source),
                None => hit.source,
            });
            if entry.title.is_none() {
                entry.title = hit.title.clone();
            }
            if entry.snippet.is_none() {
                entry.snippet = hit.snippet.clone();
            }
        }
    }

    let mut merged: Vec<Row> = scores
        .into_iter()
        .filter_map(|(id, bucket)| {
            let source = bucket.source.expect("each bucket gets a source");
            if source_filter.is_some_and(|expected| source != expected) {
                return None;
            }
            Some((
                id,
                bucket.score,
                RankScoreKind::Rrf,
                bucket.signals,
                source,
                bucket.title,
                bucket.snippet,
            ))
        })
        .collect();

    merged.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    merged.truncate(limit);
    merged
}

/// Both merges against the reference copy, under every source filter.
fn assert_matches_reference(lists: &[Vec<Appearance>], limit: usize) {
    for filter in FILTERS {
        let expected = reference_merge(lists, limit, filter);
        assert_eq!(
            entity_merge(lists, limit, filter),
            expected,
            "entity merge, limit {limit}, filter {filter:?}"
        );
        assert_eq!(
            note_merge(lists, limit, filter),
            expected,
            "note merge, limit {limit}, filter {filter:?}"
        );
    }
}

#[test]
fn ported_merges_match_the_reference_over_random_lists() {
    let mut rng = Lcg(0xfeed);
    for _ in 0..400 {
        let lists = random_lists(&mut rng);
        let limit = match rng.below(5) {
            0 => usize::MAX,
            n => n,
        };
        assert_matches_reference(&lists, limit);
    }
}

#[test]
fn ported_merges_match_the_reference_for_tied_evidence_and_late_labels() {
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

    assert_matches_reference(&lists, 10);

    let merged = entity_merge(&lists, 10, None);
    let top = &merged[0];
    assert_eq!(top.0, x);
    assert_eq!(top.3, vector(1));
    assert_eq!(top.4, SearchSource::Both);
    assert_eq!(top.5.as_deref(), Some("late"));
    assert_eq!(top.6.as_deref(), Some("late snippet"));
}

#[test]
fn ported_merges_match_the_reference_for_empty_and_one_sided_inputs() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let text_only = vec![
        appearance(a, keyword(5), SearchSource::Text, Some("a")),
        appearance(b, keyword(3), SearchSource::Text, None),
    ];
    let late = appearance(b, vector(8), SearchSource::Vector, Some("b"));
    let vector_only = vec![late];
    let cases = [
        vec![],
        vec![vec![]],
        vec![vec![], vec![]],
        vec![text_only.clone()],
        vec![vec![], text_only.clone()],
        vec![text_only.clone(), vec![]],
        vec![vector_only.clone()],
        vec![text_only, vector_only],
    ];

    for lists in &cases {
        for limit in [0, 1, 10, usize::MAX] {
            assert_matches_reference(lists, limit);
        }
    }
}

#[test]
fn ported_merges_filter_on_the_fused_source_before_cutting_to_the_limit() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let c = Uuid::from_u128(3);
    // `a` is Text on one backend and Vector on the other, so it fuses to Both and ranks first.
    // `b` and `c` tie below it and `b` wins on its smaller id.
    let lists = [
        vec![
            appearance(a, keyword(4), SearchSource::Text, None),
            appearance(b, keyword(3), SearchSource::Text, None),
        ],
        vec![
            appearance(a, vector(6), SearchSource::Vector, None),
            appearance(c, vector(5), SearchSource::Vector, None),
        ],
    ];

    let unfiltered = entity_merge(&lists, 10, None);
    assert_eq!(ids_of(&unfiltered), vec![a, b, c]);

    // `a` has a Text copy, but its fused source is Both, so the Text filter drops it before the
    // cut and the one hit kept is the next in line.
    let text = entity_merge(&lists, 1, Some(SearchSource::Text));
    assert_eq!(ids_of(&text), vec![b]);
    let vector_only = entity_merge(&lists, 1, Some(SearchSource::Vector));
    assert_eq!(ids_of(&vector_only), vec![c]);
    let both = entity_merge(&lists, 1, Some(SearchSource::Both));
    assert_eq!(ids_of(&both), vec![a]);
    let notes = note_merge(&lists, 1, Some(SearchSource::Text));
    assert_eq!(ids_of(&notes), vec![b]);

    // Filtering removes hits and never changes the score of the hits that stay.
    assert_eq!(text[0].1, unfiltered[1].1);
    assert_eq!(vector_only[0].1, unfiltered[2].1);
    assert_eq!(both[0].1, unfiltered[0].1);
    assert_eq!(both[0].1, rrf_score(1, 60) + rrf_score(1, 60));
}
