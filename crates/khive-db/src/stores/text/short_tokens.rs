use khive_score::DeterministicScore;
use khive_storage::types::{TextFilter, TextQueryMode, TextSearchHit};
use khive_storage::usage::UsageContext;
use rusqlite::Connection;
use uuid::Uuid;

use super::{build_filter_clause, count_fts_pass, sanitize_fts5_token_group, LEXICAL_BM25_RANK};

pub(super) struct Signals(Vec<String>);

impl Signals {
    pub(super) fn for_query(query: &str, mode: &TextQueryMode, top_k: u32) -> Option<Self> {
        if top_k == 0 || matches!(mode, TextQueryMode::Phrase) {
            return None;
        }
        let mut signals = Vec::new();
        for token in query.split_whitespace() {
            if !(1..=2).contains(&token.len())
                || !token.bytes().all(|byte| byte.is_ascii_alphanumeric())
                || sanitize_fts5_token_group(token).as_deref() != Some(token)
            {
                continue;
            }
            let signal = token.to_ascii_lowercase();
            if !signals.contains(&signal) {
                signals.push(signal);
            }
        }
        (!signals.is_empty()).then_some(Self(signals))
    }

    fn count(&self, title: &str, body: &str) -> usize {
        self.0
            .iter()
            .filter(|signal| {
                [title, body].into_iter().any(|field| {
                    field
                        .as_bytes()
                        .split(|byte| !byte.is_ascii_alphanumeric())
                        .any(|run| run.eq_ignore_ascii_case(signal.as_bytes()))
                })
            })
            .count()
    }
}

pub(super) enum Window {
    Ranked,
    Unranked,
    Capped(u32),
}

pub(super) struct Search<'a> {
    pub(super) filter: Option<&'a TextFilter>,
    pub(super) top_k: u32,
    pub(super) snippet_chars: usize,
    pub(super) window: Window,
}

struct Candidate {
    hit: TextSearchHit,
    raw_rank: Option<f64>,
    matches: usize,
    original_position: i64,
}

pub(super) fn search(
    conn: &Connection,
    table: &str,
    match_expr: &str,
    request: Search<'_>,
    signals: Signals,
    usage: Option<&UsageContext>,
) -> rusqlite::Result<Vec<TextSearchHit>> {
    let overfetch = request.top_k.saturating_add(request.top_k.min(64));
    let filter_start = if matches!(&request.window, Window::Capped(_)) {
        4
    } else {
        3
    };
    let (filter_clause, filter_params) = request.filter.map_or_else(
        || (String::new(), Vec::new()),
        |filter| build_filter_clause(filter, table, filter_start),
    );
    let snippet_expr = if request.snippet_chars == 0 || matches!(&request.window, Window::Unranked)
    {
        "NULL AS snippet".to_string()
    } else {
        let chars = i32::try_from(request.snippet_chars).unwrap_or(i32::MAX);
        format!("snippet({table}, 3, '', '', '...', {chars}) AS snippet")
    };
    // Only metadata is materialized. Body lookup stays on the limited physical
    // rows, never on subject IDs that may also occur in another namespace.
    let (window_sql, prefix, limit, outer_limit) = match request.window {
        Window::Ranked => (
            format!(
                "SELECT rowid AS physical_rowid, subject_id, rank AS raw_rank, title, {snippet_expr} \
                 FROM {table} WHERE {table} MATCH ?1 \
                 AND rank MATCH '{LEXICAL_BM25_RANK}'{filter_clause} \
                 ORDER BY rank LIMIT ?2"
            ),
            "WITH ".to_string(),
            overfetch,
            None,
        ),
        Window::Unranked => (
            format!(
                "SELECT rowid AS physical_rowid, subject_id, NULL AS raw_rank, title, {snippet_expr} \
                 FROM {table} WHERE {table} MATCH ?1{filter_clause} LIMIT ?2"
            ),
            "WITH ".to_string(),
            request.top_k,
            None,
        ),
        Window::Capped(gather_limit) => (
            "SELECT * FROM gathered ORDER BY raw_rank LIMIT ?3".to_string(),
            format!(
                "WITH gathered AS MATERIALIZED (\
                 SELECT rowid AS physical_rowid, subject_id, rank AS raw_rank, title, {snippet_expr} \
                 FROM {table} WHERE {table} MATCH ?1 \
                 AND rank MATCH '{LEXICAL_BM25_RANK}'{filter_clause} LIMIT ?2), "
            ),
            gather_limit,
            Some(gather_limit.min(overfetch)),
        ),
    };
    let sql = format!(
        "{prefix}observed AS MATERIALIZED (\
         SELECT row_number() OVER () AS source_ordinal, source_window.* \
         FROM ({window_sql}) AS source_window) \
         SELECT subject_id, raw_rank, title, snippet, source_ordinal, \
         (SELECT body FROM {table} WHERE rowid = observed.physical_rowid) \
         FROM observed"
    );
    let mut stmt = conn.prepare(&sql)?;
    count_fts_pass(usage);
    stmt.raw_bind_parameter(1, match_expr)?;
    stmt.raw_bind_parameter(2, i64::from(limit))?;
    if let Some(limit) = outer_limit {
        stmt.raw_bind_parameter(3, i64::from(limit))?;
    }
    for (i, param) in filter_params.iter().enumerate() {
        param
            .to_sql()
            .map(|value| stmt.raw_bind_parameter(filter_start + i, value))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))??;
    }
    let mut rows = stmt.raw_query();
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let raw_rank = row.get(1)?;
        let title: String = row.get(2)?;
        let snippet: Option<String> = row.get(3)?;
        let original_position: i64 = row.get(4)?;
        let body: String = row.get(5)?;
        let subject_id = Uuid::parse_str(&id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let matches = signals.count(&title, &body);
        candidates.push(Candidate {
            hit: TextSearchHit {
                subject_id,
                score: DeterministicScore::from_f64(1.0),
                rank: 0,
                title: (!title.is_empty()).then_some(title),
                snippet: snippet.filter(|value| !value.is_empty()),
            },
            raw_rank,
            matches,
            original_position,
        });
    }
    candidates.sort_by(|a, b| {
        let rank_order = match (a.raw_rank, b.raw_rank) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            _ => std::cmp::Ordering::Equal,
        };
        rank_order.then(a.original_position.cmp(&b.original_position))
    });
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.matches));
    candidates.truncate(request.top_k as usize);
    let moved = candidates
        .iter()
        .enumerate()
        .any(|(position, candidate)| candidate.original_position != position as i64 + 1);
    let min_rank = candidates
        .iter()
        .filter_map(|candidate| candidate.raw_rank)
        .fold(f64::INFINITY, f64::min);
    let max_rank = candidates
        .iter()
        .filter_map(|candidate| candidate.raw_rank)
        .fold(f64::NEG_INFINITY, f64::max);
    let range = max_rank - min_rank;
    let mut scores: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            let score = candidate.raw_rank.map_or(1.0, |raw_rank| {
                if range.abs() < 1e-12 {
                    1.0
                } else {
                    let t = (max_rank - raw_rank) / range;
                    0.05 + 0.95 * t
                }
            });
            DeterministicScore::from_f64(score)
        })
        .collect();
    // Keep the existing normalization's multiset while making returned ranks
    // and descending scores agree with the new order; no-move hits stay exact.
    if moved {
        scores.sort_by_key(|score| std::cmp::Reverse(*score));
    }
    Ok(candidates
        .into_iter()
        .zip(scores)
        .enumerate()
        .map(|(position, (mut candidate, score))| {
            candidate.hit.rank = position as u32 + 1;
            candidate.hit.score = score;
            candidate.hit
        })
        .collect())
}
