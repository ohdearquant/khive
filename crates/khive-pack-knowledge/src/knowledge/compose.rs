//! Hybrid section scoring for knowledge.compose (ADR-051 read-side).

use std::collections::HashMap;

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
}
