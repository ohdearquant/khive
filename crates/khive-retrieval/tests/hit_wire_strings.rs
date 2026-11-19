//! `RankScoreKind::as_str` and `SearchSource::as_str` return the strings that
//! search results carry on the wire. Each variant's string is pinned here, in
//! the crate that defines the types.
//!
//! The `match` in each helper has no wildcard arm, so a new variant does not
//! compile until its wire string is written down in that helper.

use khive_retrieval::{RankScoreKind, SearchSource};

fn assert_rank_score_kind_string(kind: RankScoreKind) {
    let expected = match kind {
        RankScoreKind::Rrf => "rrf",
        RankScoreKind::Vector => "vector",
        RankScoreKind::Keyword => "keyword",
        RankScoreKind::Weighted => "weighted",
        RankScoreKind::Union => "union",
    };
    assert_eq!(kind.as_str(), expected, "{kind:?}");
}

fn assert_search_source_string(source: SearchSource) {
    let expected = match source {
        SearchSource::Vector => "vector",
        SearchSource::Text => "text",
        SearchSource::Both => "both",
    };
    assert_eq!(source.as_str(), expected, "{source:?}");
}

#[test]
fn rank_score_kind_wire_strings_are_pinned() {
    assert_rank_score_kind_string(RankScoreKind::Rrf);
    assert_rank_score_kind_string(RankScoreKind::Vector);
    assert_rank_score_kind_string(RankScoreKind::Keyword);
    assert_rank_score_kind_string(RankScoreKind::Weighted);
    assert_rank_score_kind_string(RankScoreKind::Union);
}

#[test]
fn search_source_wire_strings_are_pinned() {
    assert_search_source_string(SearchSource::Vector);
    assert_search_source_string(SearchSource::Text);
    assert_search_source_string(SearchSource::Both);
}
