//! Hybrid section scoring for knowledge.compose (ADR-051 read-side).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_score::vector::cosine_similarity;
use khive_storage::types::{SqlStatement, SqlValue};

use super::util::{row_str, sql_err, SERVABLE_SECTION};

// ─── section record (load result) ────────────────────────────────────────────

pub(super) struct ScoredSection {
    pub id: String,
    pub atom_id: String,
    pub section_type: String,
    pub heading: String,
    pub content: String,
    pub embedding: Option<Vec<f32>>,
}

// ─── score weights ────────────────────────────────────────────────────────────

pub(super) struct ComposeScoreWeights {
    pub section_cosine: f32,
    pub section_bm25: f32,
    pub atom_cosine: f32,
    pub domain_score: f32,
    pub type_weight: f32,
}

impl Default for ComposeScoreWeights {
    fn default() -> Self {
        Self {
            section_cosine: 0.55,
            section_bm25: 0.20,
            atom_cosine: 0.10,
            domain_score: 0.10,
            type_weight: 0.05,
        }
    }
}

// ─── scoring result ───────────────────────────────────────────────────────────

pub(super) struct ScoreBreakdown {
    pub section_cosine: f32,
    pub section_bm25: f32,
    pub atom_cosine: f32,
    pub domain_score: f32,
    pub type_weight: f32,
}

pub(super) struct ComposeSectionResult {
    pub section_id: String,
    pub atom_id: String,
    pub section_type: String,
    pub heading: String,
    pub content: String,
    pub score: f32,
    pub score_breakdown: ScoreBreakdown,
}

// ─── DB load ─────────────────────────────────────────────────────────────────

pub(super) async fn load_sections(
    runtime: &KhiveRuntime,
    ns: &str,
    atom_ids: &[String],
) -> Result<HashMap<String, Vec<ScoredSection>>, RuntimeError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders: String = atom_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");

    let mut params: Vec<SqlValue> = vec![SqlValue::Text(ns.to_owned())];
    params.extend(atom_ids.iter().cloned().map(SqlValue::Text));

    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("load_sections reader", e))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, atom_id, section_type, heading, content, embedding \
                 FROM knowledge_sections \
                 WHERE namespace = ?1 \
                   AND atom_id IN ({placeholders}) \
                   AND {SERVABLE_SECTION}"
            ),
            params,
            label: None,
        })
        .await
        .map_err(|e| sql_err("load_sections query", e))?;

    let mut by_atom: HashMap<String, Vec<ScoredSection>> = HashMap::new();
    for row in &rows {
        let id = match row_str(row, "id") {
            Some(v) => v,
            None => continue,
        };
        let atom_id = match row_str(row, "atom_id") {
            Some(v) => v,
            None => continue,
        };
        let section_type = row_str(row, "section_type").unwrap_or_default();
        let heading = row_str(row, "heading").unwrap_or_default();
        let content = row_str(row, "content").unwrap_or_default();

        let embedding = match row.get("embedding") {
            Some(SqlValue::Blob(bytes)) => {
                let decoded = decode_embedding(bytes);
                if decoded.is_empty() {
                    None
                } else {
                    Some(decoded)
                }
            }
            _ => None,
        };

        by_atom
            .entry(atom_id.clone())
            .or_default()
            .push(ScoredSection {
                id,
                atom_id,
                section_type,
                heading,
                content,
                embedding,
            });
    }

    Ok(by_atom)
}

// ─── hybrid scorer ────────────────────────────────────────────────────────────

pub(super) fn score_sections(
    raw_query: &str,
    query_embedding: &[f32],
    atom_cosine_scores: &HashMap<String, f32>,
    sections: &HashMap<String, Vec<ScoredSection>>,
    domain_scores: &HashMap<String, f32>,
    type_weights: &HashMap<String, f32>,
    weights: &ComposeScoreWeights,
) -> Result<Vec<ComposeSectionResult>, RuntimeError> {
    let flat: Vec<&ScoredSection> = sections.values().flat_map(|secs| secs.iter()).collect();

    if flat.is_empty() {
        return Ok(Vec::new());
    }

    let doc_pairs: Vec<(&str, &str)> = flat
        .iter()
        .map(|s| (s.heading.as_str(), s.content.as_str()))
        .collect();
    let query_terms = tokenize_checked(raw_query)?;
    let bm25_raw = compute_bm25_scores(&query_terms, &doc_pairs)?;

    let max_bm25 = bm25_raw.iter().cloned().fold(0.0f32, f32::max);

    let mut results: Vec<ComposeSectionResult> = Vec::with_capacity(flat.len());
    for (section, &bm25_unnorm) in flat.iter().zip(bm25_raw.iter()) {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let sec_cos = match &section.embedding {
            Some(emb) if !emb.is_empty() => cosine_similarity(query_embedding, emb, true).max(0.0),
            _ => 0.0,
        };

        let atom_cos = atom_cosine_scores
            .get(&section.atom_id)
            .copied()
            .unwrap_or(0.0)
            .max(0.0);

        let dom = domain_scores
            .get(&section.atom_id)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);

        let type_w = type_weights
            .get(section.section_type.as_str())
            .copied()
            .unwrap_or(0.05)
            .clamp(0.0, 1.0);

        let bm25_norm = if max_bm25 > 0.0 {
            (bm25_unnorm / max_bm25).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let score = weights.section_cosine * sec_cos
            + weights.section_bm25 * bm25_norm
            + weights.atom_cosine * atom_cos
            + weights.domain_score * dom
            + weights.type_weight * type_w;

        results.push(ComposeSectionResult {
            section_id: section.id.clone(),
            atom_id: section.atom_id.clone(),
            section_type: section.section_type.clone(),
            heading: section.heading.clone(),
            content: section.content.clone(),
            score,
            score_breakdown: ScoreBreakdown {
                section_cosine: sec_cos,
                section_bm25: bm25_norm,
                atom_cosine: atom_cos,
                domain_score: dom,
                type_weight: type_w,
            },
        });
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.section_id.cmp(&b.section_id))
    });
    Ok(results)
}

// ─── BM25 over candidate set ──────────────────────────────────────────────────

// Keep candidate-local BM25: khive-bm25 caps TF at 255 and lacks a request
// deadline hook. The separate TF-IDF field scorer also has a bigram bonus
// with no shared-index counterpart (#4492).

#[derive(Clone, Copy)]
enum Bm25Work {
    TokenVisit,
    FrequencyLookup,
}

struct Bm25Document {
    length: usize,
    frequencies: HashMap<String, usize>,
}

fn compute_bm25_scores(
    query_terms: &[String],
    sections: &[(&str, &str)],
) -> Result<Vec<f32>, RuntimeError> {
    compute_bm25_scores_observed(query_terms, sections, |_| {})
}

fn compute_bm25_scores_observed(
    query_terms: &[String],
    sections: &[(&str, &str)],
    mut observe: impl FnMut(Bm25Work),
) -> Result<Vec<f32>, RuntimeError> {
    const K1: f32 = 1.5;
    const B: f32 = 0.75;

    if sections.is_empty() || query_terms.is_empty() {
        return Ok(vec![0.0; sections.len()]);
    }

    let mut docs = Vec::with_capacity(sections.len());
    for (heading, content) in sections {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let mut text = String::with_capacity(heading.len() + content.len() + 1);
        text.push_str(heading);
        text.push(' ');
        text.push_str(content);
        let tokens = tokenize_checked(&text)?;
        let length = tokens.len();
        let mut frequencies = HashMap::new();
        for (index, token) in tokens.into_iter().enumerate() {
            if index.is_multiple_of(4_096) {
                khive_storage::ensure_request_read_active("knowledge.compose")?;
            }
            observe(Bm25Work::TokenVisit);
            *frequencies.entry(token).or_insert(0usize) += 1;
        }
        docs.push(Bm25Document {
            length,
            frequencies,
        });
    }

    let n = docs.len() as f32;
    let avg_dl = docs.iter().map(|d| d.length as f32).sum::<f32>() / n;

    let mut scores = vec![0.0f32; docs.len()];
    // Query occurrences retain their original addition order, including repeats.
    for term in query_terms {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let df = docs
            .iter()
            .filter(|doc| {
                observe(Bm25Work::FrequencyLookup);
                doc.frequencies.contains_key(term)
            })
            .count() as f32;
        if df == 0.0 {
            continue;
        }
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

        for (i, doc) in docs.iter().enumerate() {
            if i.is_multiple_of(64) {
                khive_storage::ensure_request_read_active("knowledge.compose")?;
            }
            observe(Bm25Work::FrequencyLookup);
            let tf = doc.frequencies.get(term).copied().unwrap_or(0) as f32;
            if tf == 0.0 {
                continue;
            }
            let dl = doc.length as f32;
            let tf_norm = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
            scores[i] += idf * tf_norm;
        }
    }

    Ok(scores)
}

#[cfg(test)]
#[path = "compose_bm25_work_tests.rs"]
mod bm25_work_tests;

// ─── pure helpers ─────────────────────────────────────────────────────────────

pub(super) fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    khive_storage::decode_f32_le(blob).unwrap_or_default()
}

fn tokenize_checked(text: &str) -> Result<Vec<String>, RuntimeError> {
    khive_storage::ensure_request_read_active("knowledge.compose")?;
    let mut terms = Vec::new();
    let mut term = String::new();
    for (index, byte) in text.bytes().enumerate() {
        if index.is_multiple_of(4_096) {
            khive_storage::ensure_request_read_active("knowledge.compose")?;
        }
        if byte.is_ascii_alphanumeric() {
            term.push((byte as char).to_ascii_lowercase());
        } else if term.len() >= 2 {
            terms.push(std::mem::take(&mut term));
        } else {
            term.clear();
        }
    }
    if term.len() >= 2 {
        terms.push(term);
    }
    Ok(terms)
}

// ─── slow-request / abandonment observability (#887) ─────────────────────────
//
// The daemon had zero compose-path evidence during a heavy-load window where
// two consecutive `knowledge.compose` calls hit the 300s client-side MCP
// timeout — no start/finish record, no duration, no timeout error. `rerank`
// is embedder-CPU-bound, so load starvation was the working hypothesis, but
// nothing in the log could confirm or refute it. `ComposeTiming` measures
// per-stage elapsed time unconditionally (unlike the opt-in
// `KHIVE_RECALL_PROFILE` eprintln profiler in the memory pack) and logs via
// `tracing::warn!`, matching the WARN-on-anomaly style used elsewhere in this
// crate (see `index_handler.rs`, `vamana.rs`).
//
// PR #915 identified that a phase only entered the reported breakdown once
// its work finished, so the exact case this exists to
// diagnose (a phase that stalls and then errors, is cancelled, or is
// abandoned by a disconnected client) logged `phases=[]`, omitting the one
// phase an on-call engineer needed. `begin(phase)` now opens the phase
// *before* its (possibly fallible, possibly long-running) work starts; both
// `finish` and `Drop` flush whatever phase is still open — `last..now` — into
// that phase's bucket before emitting, so an in-flight phase is never lost.
//
// Further validation for PR #915 found two follow-ups, both addressed here
// and in `search.rs`: (1) the regression tests called the private
// `flush_active` manually before `finish`/`drop`, so they exercised the
// flush logic but not the actual call sites that must invoke it — `finish`
// now returns its post-flush breakdown so a test can assert on the real call
// site's output directly, and the `Drop` path is covered by a `tracing`
// capture subscriber (see the test module). (2) `compose()` was calling
// `finish` before constructing its response `Value` on three routine return
// paths, so the response-JSON assembly and the trailing bit of whichever
// phase was active went unattributed; every routine return now builds its
// response into a local binding, calls `finish`, then returns it, so `finish`
// is genuinely the last thing that happens on every routine path — the flush
// picks up the tail of the final phase (including the response-JSON
// construction), not just the last explicitly `begin`-marked stage.

/// Compose requests whose total handler time reaches this are logged at WARN
/// with a per-phase breakdown. 10s matches the example threshold named in
/// #887: well above a healthy compose (sub-second in the common case) but
/// far short of the 300s client-side MCP timeout that made the original
/// incident unattributable.
pub(super) const COMPOSE_SLOW_THRESHOLD_MS: u64 = 10_000;

/// The fixed, closed set of `compose` stages timed by [`ComposeTiming`].
///
/// Compose's actual call sequence interleaves two DB fetches (domain/atom
/// resolution, then section-body load) and two embedding reranks (atom-level,
/// then section-level); `Fetch` and `Rerank` are each opened twice and
/// accumulate across both occurrences rather than getting split into four
/// differently-named buckets, matching how #887 asked for the breakdown
/// (`suggest`/`fetch`/`rerank`/`trim`).
///
/// A closed enum backing a fixed-size array — rather than `Vec<(&str, _)>` —
/// removes the heap allocation and linear name scan `ComposeTiming` would
/// otherwise pay on every valid request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Suggest,
    Fetch,
    Rerank,
    Trim,
}

impl Phase {
    const COUNT: usize = 4;
    const ALL: [Phase; Self::COUNT] = [Phase::Suggest, Phase::Fetch, Phase::Rerank, Phase::Trim];

    fn name(self) -> &'static str {
        match self {
            Phase::Suggest => "suggest",
            Phase::Fetch => "fetch",
            Phase::Rerank => "rerank",
            Phase::Trim => "trim",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Per-stage elapsed-time tracker for `knowledge.compose`.
///
/// Ordering invariant: `begin(phase)` must be called before that phase's
/// work starts (before any `.await` it covers), and `finish` must be the
/// last call on every return path (after the response `Value` is built).
/// `finish`/`Drop` both flush the active phase's partial duration -- `Drop`
/// covers the cancelled-mid-poll case with a distinct "abandoned" WARN so a
/// request that never responds is not silently invisible. See
/// crates/khive-pack-knowledge/docs/api/compose-timing.md for the full
/// call-ordering contract and the `query_bytes` eager-storage rationale.
pub(super) struct ComposeTiming {
    start: Instant,
    last: Instant,
    phase_totals: [Duration; Phase::COUNT],
    active_phase: Option<Phase>,
    query_bytes: usize,
    is_auto: bool,
    completed: bool,
}

impl ComposeTiming {
    pub(super) fn start(query: &str, is_auto: bool) -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
            phase_totals: [Duration::ZERO; Phase::COUNT],
            active_phase: None,
            query_bytes: query.len(),
            is_auto,
            completed: false,
        }
    }

    /// Closes the currently active phase (if any) into its accumulated
    /// total, then opens `phase` as the new active phase. Call this
    /// immediately before starting the phase's work — including before any
    /// `.await` — not after it completes.
    pub(super) fn begin(&mut self, phase: Phase) -> Result<(), RuntimeError> {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let now = Instant::now();
        if let Some(prev) = self.active_phase {
            self.phase_totals[prev.index()] += now.duration_since(self.last);
        }
        self.active_phase = Some(phase);
        self.last = now;
        Ok(())
    }

    /// Folds `last..now` into whichever phase is still active, so a phase
    /// that never reached its own `begin(next_phase)` call (because the
    /// request errored, was cancelled, or was dropped mid-phase) is still
    /// represented in the breakdown instead of reading as zero/absent.
    fn flush_active(&mut self) {
        let now = Instant::now();
        if let Some(phase) = self.active_phase {
            self.phase_totals[phase.index()] += now.duration_since(self.last);
            self.last = now;
        }
    }

    fn phase_ms(&self) -> [(&'static str, u64); Phase::COUNT] {
        let mut out = [("", 0u64); Phase::COUNT];
        for p in Phase::ALL {
            out[p.index()] = (p.name(), self.phase_totals[p.index()].as_millis() as u64);
        }
        out
    }

    /// Consumes the tracker: flushes any still-active phase, marks it
    /// complete so `Drop` does not also log an "abandoned" warning, and
    /// emits the slow-request WARN if the total reached
    /// [`COMPOSE_SLOW_THRESHOLD_MS`]. Returns the post-flush per-phase
    /// breakdown (mainly for test assertions — see
    /// crates/khive-pack-knowledge/docs/api/compose-timing.md).
    pub(super) fn finish(mut self, atom_count: usize) -> [(&'static str, u64); Phase::COUNT] {
        self.flush_active();
        self.completed = true;
        let breakdown = self.phase_ms();
        let total_ms = self.start.elapsed().as_millis() as u64;
        if total_ms >= COMPOSE_SLOW_THRESHOLD_MS {
            tracing::warn!(
                total_ms,
                threshold_ms = COMPOSE_SLOW_THRESHOLD_MS,
                phases = ?breakdown,
                atom_count,
                query_bytes = self.query_bytes,
                is_auto = self.is_auto,
                "knowledge.compose exceeded slow-request threshold"
            );
        }
        breakdown
    }
}

impl Drop for ComposeTiming {
    fn drop(&mut self) {
        if !self.completed {
            self.flush_active();
            tracing::warn!(
                elapsed_ms = self.start.elapsed().as_millis() as u64,
                phases = ?self.phase_ms(),
                query_bytes = self.query_bytes,
                is_auto = self.is_auto,
                "knowledge.compose request abandoned before completion \
                 (client disconnect, cancellation, or daemon shutdown)"
            );
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "compose_tests.rs"]
mod tests;
