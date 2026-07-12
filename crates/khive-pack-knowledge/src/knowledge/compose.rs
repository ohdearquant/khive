//! Hybrid section scoring for knowledge.compose (ADR-051 read-side).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::util::{row_str, sql_err};

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
                   AND atom_id IN ({placeholders})"
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
) -> Vec<ComposeSectionResult> {
    let flat: Vec<&ScoredSection> = sections.values().flat_map(|secs| secs.iter()).collect();

    if flat.is_empty() {
        return Vec::new();
    }

    let doc_pairs: Vec<(&str, &str)> = flat
        .iter()
        .map(|s| (s.heading.as_str(), s.content.as_str()))
        .collect();
    let query_terms = tokenize(raw_query);
    let bm25_raw = compute_bm25_scores(&query_terms, &doc_pairs);

    let max_bm25 = bm25_raw.iter().cloned().fold(0.0f32, f32::max);

    let mut results: Vec<ComposeSectionResult> = flat
        .iter()
        .zip(bm25_raw.iter())
        .map(|(section, &bm25_unnorm)| {
            let sec_cos = match &section.embedding {
                Some(emb) if !emb.is_empty() => cosine_similarity(query_embedding, emb).max(0.0),
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

            ComposeSectionResult {
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
            }
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.section_id.cmp(&b.section_id))
    });
    results
}

// ─── BM25 over candidate set ──────────────────────────────────────────────────

fn compute_bm25_scores(query_terms: &[String], sections: &[(&str, &str)]) -> Vec<f32> {
    const K1: f32 = 1.5;
    const B: f32 = 0.75;

    if sections.is_empty() || query_terms.is_empty() {
        return vec![0.0; sections.len()];
    }

    let docs: Vec<Vec<String>> = sections
        .iter()
        .map(|(heading, content)| tokenize(&format!("{heading} {content}")))
        .collect();

    let n = docs.len() as f32;
    let avg_dl = docs.iter().map(|d| d.len() as f32).sum::<f32>() / n;

    let mut scores = vec![0.0f32; docs.len()];
    for term in query_terms {
        let df = docs.iter().filter(|d| d.iter().any(|t| t == term)).count() as f32;
        if df == 0.0 {
            continue;
        }
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

        for (i, doc) in docs.iter().enumerate() {
            let tf = doc.iter().filter(|t| *t == term).count() as f32;
            if tf == 0.0 {
                continue;
            }
            let dl = doc.len() as f32;
            let tf_norm = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
            scores[i] += idf * tf_norm;
        }
    }

    scores
}

// ─── pure helpers ─────────────────────────────────────────────────────────────

pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-8 {
        0.0
    } else {
        (dot / denom).clamp(-1.0, 1.0)
    }
}

pub(super) fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        return Vec::new();
    }
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(str::to_string)
        .collect()
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
// Round-1 codex review (#915) flagged that a phase only entered the reported
// breakdown once its work finished — so the exact case this exists to
// diagnose (a phase that stalls and then errors, is cancelled, or is
// abandoned by a disconnected client) logged `phases=[]`, omitting the one
// phase an on-call engineer needed. `begin(phase)` now opens the phase
// *before* its (possibly fallible, possibly long-running) work starts; both
// `finish` and `Drop` flush whatever phase is still open — `last..now` — into
// that phase's bucket before emitting, so an in-flight phase is never lost.
// The same flush also picks up the tail of the final phase (response-JSON
// construction after the last measured stage), which a pre-`finish` mark
// would otherwise miss.

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
/// otherwise pay on every valid request (round-1 codex review, Low finding).
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
/// `begin(phase)` must be called *before* that phase's work starts (in
/// particular, before any `.await` the phase covers) — it closes out
/// whichever phase was previously active (accumulating `last..now` into it)
/// and opens `phase` as the new active phase. Because the active phase is
/// known at every instant, `finish` and `Drop` can both flush an in-flight
/// phase's partial duration into the breakdown rather than silently omitting
/// it, which is what a slow-then-failing or cancelled-mid-phase request
/// needs (round-1 codex review, Medium finding).
///
/// `finish` must be the last thing called on every return path that
/// completes the request (success or a business-logic error): it flushes the
/// active phase, flags the timing as complete, and, if the total reaches
/// [`COMPOSE_SLOW_THRESHOLD_MS`], emits the slow-request WARN. If `finish` is
/// never reached — because the enclosing future was dropped mid-poll (client
/// disconnect, cancellation, or daemon shutdown drain) — `Drop` performs the
/// same flush and emits a distinct "abandoned" WARN, so a request that never
/// produces a response is not silently invisible.
///
/// `query_bytes` records the query's UTF-8 *byte* length, not a char count —
/// `str::len()` reads a value the string already carries (O(1)), unlike
/// `.chars().count()`'s O(n) UTF-8 walk. Because it is O(1), there is nothing
/// to gain by deferring it to the (rare) emission path the way the Low
/// finding suggested for a genuinely O(n) computation: storing it eagerly
/// costs the same as storing it lazily, and eager storage avoids holding a
/// borrow of the caller's query string for the tracker's entire lifetime —
/// `compose()` moves `raw_query` into the response body before calling
/// `finish()`, so a borrowing field would not compile (round-1 codex review,
/// Low finding).
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
    pub(super) fn begin(&mut self, phase: Phase) {
        let now = Instant::now();
        if let Some(prev) = self.active_phase {
            self.phase_totals[prev.index()] += now.duration_since(self.last);
        }
        self.active_phase = Some(phase);
        self.last = now;
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
    /// complete so `Drop` does not also log an "abandoned" warning for a
    /// request that finished normally, and emits the slow-request WARN if
    /// the total reached [`COMPOSE_SLOW_THRESHOLD_MS`].
    pub(super) fn finish(mut self, atom_count: usize) {
        self.flush_active();
        self.completed = true;
        let total_ms = self.start.elapsed().as_millis() as u64;
        if total_ms >= COMPOSE_SLOW_THRESHOLD_MS {
            tracing::warn!(
                total_ms,
                threshold_ms = COMPOSE_SLOW_THRESHOLD_MS,
                phases = ?self.phase_ms(),
                atom_count,
                query_bytes = self.query_bytes,
                is_auto = self.is_auto,
                "knowledge.compose exceeded slow-request threshold"
            );
        }
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
mod tests {
    use super::*;

    #[test]
    fn decode_embedding_round_trips() {
        let values: Vec<f32> = vec![1.0, -0.5, 0.0, 42.5];
        let blob: Vec<u8> = values.iter().flat_map(|f| f.to_le_bytes()).collect();
        let decoded = decode_embedding(&blob);
        assert_eq!(decoded.len(), 4);
        for (a, b) in values.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 1e-6, "mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn decode_embedding_rejects_misaligned_blob() {
        let blob = vec![0u8; 7];
        assert!(decode_embedding(&blob).is_empty());
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_mismatched_lengths_returns_zero() {
        let a = vec![1.0f32, 2.0];
        let b = vec![1.0f32];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn bm25_scores_higher_for_matching_doc() {
        let terms = vec!["rust".to_string(), "memory".to_string()];
        let docs = &[
            (
                "Rust memory model",
                "Rust ownership and memory safety rules",
            ),
            ("Python lists", "Python list operations and indexing"),
        ];
        let scores = compute_bm25_scores(&terms, docs);
        assert_eq!(scores.len(), 2);
        assert!(scores[0] > scores[1], "matching doc must score higher");
    }

    #[test]
    fn bm25_empty_terms_returns_zeros() {
        let docs = &[("heading", "content"), ("heading2", "content2")];
        let scores = compute_bm25_scores(&[], docs);
        assert!(scores.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn score_sections_sorted_desc() {
        let query_emb: Vec<f32> = vec![1.0, 0.0];
        let mut atom_cos: HashMap<String, f32> = HashMap::new();
        atom_cos.insert("a1".to_string(), 0.9);

        let sec = ScoredSection {
            id: "s1".to_string(),
            atom_id: "a1".to_string(),
            section_type: "overview".to_string(),
            heading: "Overview".to_string(),
            content: "introduction to the topic".to_string(),
            embedding: Some(vec![1.0, 0.0]),
        };
        let sec2 = ScoredSection {
            id: "s2".to_string(),
            atom_id: "a1".to_string(),
            section_type: "references".to_string(),
            heading: "References".to_string(),
            content: "unrelated bibliography content".to_string(),
            embedding: Some(vec![0.0, 1.0]),
        };

        let mut sections: HashMap<String, Vec<ScoredSection>> = HashMap::new();
        sections.insert("a1".to_string(), vec![sec, sec2]);

        let domain_scores: HashMap<String, f32> = HashMap::new();
        let type_weights: HashMap<String, f32> = HashMap::new();

        let results = score_sections(
            "overview introduction topic",
            &query_emb,
            &atom_cos,
            &sections,
            &domain_scores,
            &type_weights,
            &ComposeScoreWeights::default(),
        );

        assert_eq!(results.len(), 2);
        assert!(results[0].score >= results[1].score, "must be sorted desc");
        assert_eq!(results[0].section_id, "s1");
    }

    #[test]
    fn default_weights_sum_to_one() {
        let w = ComposeScoreWeights::default();
        let sum =
            w.section_cosine + w.section_bm25 + w.atom_cosine + w.domain_score + w.type_weight;
        assert!((sum - 1.0).abs() < 1e-6, "weights sum to {sum}");
    }

    #[test]
    fn unembedded_section_still_scored_via_keyword_signals() {
        let query_emb: Vec<f32> = vec![1.0, 0.0];
        let mut atom_cos: HashMap<String, f32> = HashMap::new();
        atom_cos.insert("a1".to_string(), 0.8);

        let embedded = ScoredSection {
            id: "s1".to_string(),
            atom_id: "a1".to_string(),
            section_type: "overview".to_string(),
            heading: "Overview".to_string(),
            content: "topic introduction".to_string(),
            embedding: Some(vec![1.0, 0.0]),
        };
        let unembedded = ScoredSection {
            id: "s2".to_string(),
            atom_id: "a1".to_string(),
            section_type: "details".to_string(),
            heading: "Details".to_string(),
            content: "topic details and explanation".to_string(),
            embedding: None,
        };

        let mut sections: HashMap<String, Vec<ScoredSection>> = HashMap::new();
        sections.insert("a1".to_string(), vec![embedded, unembedded]);

        let domain_scores: HashMap<String, f32> = HashMap::new();
        let type_weights: HashMap<String, f32> = HashMap::new();

        let results = score_sections(
            "topic overview",
            &query_emb,
            &atom_cos,
            &sections,
            &domain_scores,
            &type_weights,
            &ComposeScoreWeights::default(),
        );

        assert_eq!(results.len(), 2, "both sections must be scored");
        let unembedded_result = results.iter().find(|r| r.section_id == "s2").unwrap();
        assert_eq!(
            unembedded_result.score_breakdown.section_cosine, 0.0,
            "unembedded section_cosine must be 0"
        );
        assert!(
            unembedded_result.score > 0.0,
            "unembedded section must still have positive score from other signals"
        );
    }

    // ── ComposeTiming (#887) ────────────────────────────────────────────────

    #[test]
    fn slow_threshold_is_sane() {
        // Sanity-bounds the const rather than pinning it to exactly 10_000,
        // so a deliberate future retune doesn't require touching the test —
        // but a typo (e.g. dropping three zeros) still fails loudly. Both
        // sides are compile-time-constant, so `clippy::assertions_on_constants`
        // requires the `const { }` wrapper (this becomes a build-time check,
        // which is strictly stronger than a runtime test assertion).
        const {
            assert!(
                COMPOSE_SLOW_THRESHOLD_MS >= 1_000,
                "threshold must be well above a healthy sub-second compose"
            );
        }
        const {
            assert!(
                COMPOSE_SLOW_THRESHOLD_MS <= 60_000,
                "threshold must be well under the 300s client-side MCP timeout \
                 (#887) to give advance warning"
            );
        }
    }

    #[test]
    fn begin_accumulates_duration_under_repeated_phase_names() {
        let mut t = ComposeTiming::start("test query", false);
        t.begin(Phase::Suggest);
        std::thread::sleep(Duration::from_millis(2));
        // Second DB fetch (e.g. load_sections) accumulates into the same
        // Fetch bucket instead of overwriting or duplicating it.
        t.begin(Phase::Fetch);
        std::thread::sleep(Duration::from_millis(2));
        t.begin(Phase::Rerank);
        std::thread::sleep(Duration::from_millis(2));
        t.begin(Phase::Fetch);
        std::thread::sleep(Duration::from_millis(2));
        t.begin(Phase::Trim);

        let fetch_ms = t.phase_totals[Phase::Fetch.index()].as_millis();
        let rerank_ms = t.phase_totals[Phase::Rerank.index()].as_millis();
        assert!(
            fetch_ms >= 4,
            "accumulated fetch time must cover both begin(Fetch) spans (~4ms), got {fetch_ms}ms"
        );
        assert!(
            rerank_ms >= 2,
            "rerank bucket must cover its single span, got {rerank_ms}ms"
        );

        t.finish(3);
    }

    #[test]
    fn finish_flushes_the_still_active_phase() {
        // Regression for round-1 codex review (#915, Medium): a phase that
        // never reaches its own `begin(next_phase)` — because the request
        // errors, is cancelled, or `finish` is simply called mid-phase — must
        // still show up in the breakdown with nonzero duration rather than
        // being silently omitted (the exact bug: a slow, then-failing
        // `suggest` used to log `phases=[]`). `finish` consumes the tracker
        // and emits no return value, so this exercises the same flush it
        // performs internally (`flush_active`, called first inside `finish`)
        // directly on `phase_totals` — no log-capture harness exists in this
        // crate (#887 scope note).
        let mut t = ComposeTiming::start("test query", true);
        t.begin(Phase::Suggest);
        std::thread::sleep(Duration::from_millis(5));
        // No begin(Phase::Fetch) — Suggest is still the active phase.
        t.flush_active();
        let suggest_ms = t.phase_totals[Phase::Suggest.index()].as_millis();
        assert!(
            suggest_ms >= 5,
            "in-flight Suggest phase must be flushed with nonzero duration, got {suggest_ms}ms"
        );
        t.finish(0);
    }

    #[test]
    fn drop_without_finish_flushes_the_active_phase_and_does_not_panic() {
        // Simulates an abandoned request (future dropped mid-phase, e.g.
        // client disconnect or cancellation) — the phase active at drop time
        // must be flushed into the breakdown (not lost) and drop must not
        // panic even though `completed` was never set.
        let mut t = ComposeTiming::start("abandoned", false);
        t.begin(Phase::Rerank);
        std::thread::sleep(Duration::from_millis(5));
        // `flush_active` is idempotent (repeated calls just extend `last`),
        // so calling it here to observe the pre-drop state does not change
        // what Drop itself will flush.
        t.flush_active();
        let rerank_ms = t.phase_totals[Phase::Rerank.index()].as_millis();
        assert!(
            rerank_ms >= 5,
            "in-flight Rerank phase must be flushed before drop, got {rerank_ms}ms"
        );
        drop(t);
    }

    #[test]
    fn begin_with_no_prior_active_phase_does_not_panic() {
        // The very first `begin` call has nothing to flush.
        let t = ComposeTiming::start("q", true);
        drop(t);
    }

    #[test]
    fn query_bytes_is_byte_length_not_char_length() {
        // Multi-byte UTF-8 query: byte length (the documented, O(1) choice)
        // must not silently become a char count.
        let t = ComposeTiming::start("héllo wörld", false);
        assert_eq!(
            t.query_bytes, 13,
            "2 non-ASCII chars, each 2 bytes in UTF-8"
        );
        t.finish(0);
    }
}
