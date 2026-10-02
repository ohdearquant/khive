//! Bitwise parity with the original candidate-local BM25 and actual work bounds.

use super::*;

// Literal scoring loops from public d17642c7, independent of the frequency maps.
fn baseline_bm25_scores(
    query_terms: &[String],
    sections: &[(&str, &str)],
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
        docs.push(tokenize_checked(&text)?);
    }

    let n = docs.len() as f32;
    let avg_dl = docs.iter().map(|d| d.len() as f32).sum::<f32>() / n;

    let mut scores = vec![0.0f32; docs.len()];
    for term in query_terms {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let df = docs.iter().filter(|d| d.iter().any(|t| t == term)).count() as f32;
        if df == 0.0 {
            continue;
        }
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

        for (i, doc) in docs.iter().enumerate() {
            if i.is_multiple_of(64) {
                khive_storage::ensure_request_read_active("knowledge.compose")?;
            }
            let tf = doc.iter().filter(|t| *t == term).count() as f32;
            if tf == 0.0 {
                continue;
            }
            let dl = doc.len() as f32;
            let tf_norm = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
            scores[i] += idf * tf_norm;
        }
    }

    Ok(scores)
}

fn baseline_score_sections(
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
    let bm25_raw = baseline_bm25_scores(&query_terms, &doc_pairs)?;

    let max_bm25 = bm25_raw.iter().cloned().fold(0.0f32, f32::max);

    let mut results: Vec<ComposeSectionResult> = Vec::with_capacity(flat.len());
    for (section, &bm25_unnorm) in flat.iter().zip(bm25_raw.iter()) {
        khive_storage::ensure_request_read_active("knowledge.compose")?;
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

#[derive(Default, Debug)]
struct Work {
    token_visits: usize,
    frequency_lookups: usize,
}

impl Work {
    fn observe(&mut self, work: Bm25Work) {
        match work {
            Bm25Work::TokenVisit => self.token_visits += 1,
            Bm25Work::FrequencyLookup => self.frequency_lookups += 1,
        }
    }
}

fn assert_parity(query: &[String], docs: &[(&str, &str)]) {
    let expected = baseline_bm25_scores(query, docs).unwrap();
    let actual = compute_bm25_scores(query, docs).unwrap();
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(actual.to_bits(), expected.to_bits(), "document {index}");
    }
}

#[test]
fn bm25_token_boundaries_and_empty_inputs_bit_parity() {
    let docs = [
        ("", ""),
        ("RUST alpha", "alpha_42; q r 9; βeta élan 中文"),
        ("boundary", "word Rust rust! x-y ab cd"),
        (
            "rust",
            "different length with absent tokens and punctuation...",
        ),
    ];
    for raw in ["", "x y β", "Rust 42", "boundaryword boundary word", "none"] {
        let query = tokenize_checked(raw).unwrap();
        assert_parity(&query, &docs);
        assert_parity(&query, &[]);
    }
    let mut work = Work::default();
    assert_eq!(
        compute_bm25_scores_observed(&[], &docs, |event| work.observe(event)).unwrap(),
        vec![0.0; docs.len()]
    );
    assert_eq!(work.token_visits, 0, "empty query must not build counts");
    assert_eq!(work.frequency_lookups, 0);
}

#[test]
fn bm25_repeated_query_occurrences_bit_parity() {
    let docs = [
        ("rust", "rust alpha beta"),
        ("", "rust rust rust rust alpha"),
        ("missing", "other"),
    ];
    let query = tokenize_checked("rust rust alpha rust").unwrap();
    assert_parity(&query, &docs);
    let once = compute_bm25_scores(&tokenize_checked("rust alpha").unwrap(), &docs).unwrap();
    let repeated = baseline_bm25_scores(&query, &docs).unwrap();
    assert_ne!(
        once[0].to_bits(),
        repeated[0].to_bits(),
        "repetition must affect the actual score"
    );
}

#[test]
fn bm25_single_query_bit_parity() {
    assert_parity(
        &tokenize_checked("rust").unwrap(),
        &[("rust", "alpha beta"), ("", "other rust rust"), ("", "")],
    );
}

#[test]
fn bm25_uncapped_term_frequency_bit_parity() {
    let repeated = "needle ".repeat(4_097);
    assert_parity(
        &tokenize_checked("needle").unwrap(),
        &[("", &repeated), ("", "needle other"), ("", "different")],
    );
}

#[test]
fn bm25_real_token_work_stays_linear_in_corpus() {
    let content = format!("{}other", "needle ".repeat(1_024));
    let docs: Vec<(&str, &str)> = (0..8).map(|_| ("", content.as_str())).collect();
    let query: Vec<String> = (0..32)
        .map(|index| {
            if index % 4 == 0 {
                "needle".to_string()
            } else {
                format!("absent{index}")
            }
        })
        .collect();
    let tokens = docs.len() * 1_025;
    let mut work = Work::default();
    let actual = compute_bm25_scores_observed(&query, &docs, |event| work.observe(event)).unwrap();
    let expected = baseline_bm25_scores(&query, &docs).unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|score| score.to_bits())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|score| score.to_bits())
            .collect::<Vec<_>>()
    );
    assert!(
        work.token_visits <= 2 * tokens,
        "actual token visits exceed the corpus bound: {work:?}"
    );
    assert!(
        work.frequency_lookups >= query.len() * docs.len(),
        "must observe the actual lookups: {work:?}"
    );
    assert!(
        work.frequency_lookups <= 2 * query.len() * docs.len(),
        "lookup work must depend on documents and query occurrences: {work:?}"
    );
}

#[tokio::test]
async fn bm25_counting_observes_active_request_cancellation() {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    khive_storage::scope_request_read_cancellation(cancel_rx, async move {
        let content = "needle ".repeat(50_000);
        let mut visits = 0usize;
        let result =
            compute_bm25_scores_observed(&["needle".to_string()], &[("", &content)], |event| {
                if matches!(event, Bm25Work::TokenVisit) {
                    visits += 1;
                    if visits == 1 {
                        cancel_tx.send(true).unwrap();
                    }
                }
            });
        assert!(
            result.is_err(),
            "cancelled construction must not produce scores"
        );
        assert!(
            visits > 0 && visits <= 4_096,
            "cancellation must stop the real long counting loop: {visits}"
        );
    })
    .await;
}

fn result_bytes(results: &[ComposeSectionResult]) -> Vec<u8> {
    let values: Vec<_> = results.iter().map(|result| serde_json::json!({
        "id": result.section_id, "atom": result.atom_id, "kind": result.section_type,
        "heading": result.heading, "content": result.content,
        "score_bits": result.score.to_bits(),
        "breakdown_bits": [result.score_breakdown.section_cosine.to_bits(), result.score_breakdown.section_bm25.to_bits(), result.score_breakdown.atom_cosine.to_bits(), result.score_breakdown.domain_score.to_bits(), result.score_breakdown.type_weight.to_bits()],
    })).collect();
    serde_json::to_vec(&values).unwrap()
}

#[test]
fn compose_result_metadata_bits_and_order_match_original_scorer() {
    let section = |id: &str, heading: &str, content: &str, embedding| ScoredSection {
        id: id.to_string(),
        atom_id: "atom".to_string(),
        section_type: "definition".to_string(),
        heading: heading.to_string(),
        content: content.to_string(),
        embedding,
    };
    let sections = HashMap::from([(
        "atom".to_string(),
        vec![
            section("z", "rust", "alpha rust", Some(vec![1.0, 0.0])),
            section("a", "rust", "alpha rust", Some(vec![1.0, 0.0])),
            section("empty", "", "", Some(vec![0.0, 0.0])),
            section("missing", "other", "different tokens", None),
            section("dimension", "alpha", "rust other", Some(vec![1.0])),
        ],
    )]);
    let atoms = HashMap::from([("atom".to_string(), 0.3)]);
    let domains = HashMap::from([("atom".to_string(), 1.5)]);
    let types = HashMap::from([("definition".to_string(), 0.25)]);
    let weights = ComposeScoreWeights::default();
    for query in ["rust rust alpha", "absent", ""] {
        let actual = score_sections(
            query,
            &[1.0, 0.0],
            &atoms,
            &sections,
            &domains,
            &types,
            &weights,
        )
        .unwrap();
        let expected = baseline_score_sections(
            query,
            &[1.0, 0.0],
            &atoms,
            &sections,
            &domains,
            &types,
            &weights,
        )
        .unwrap();
        assert_eq!(
            result_bytes(&actual),
            result_bytes(&expected),
            "query {query}"
        );
        let ids: Vec<_> = actual
            .iter()
            .map(|section| section.section_id.as_str())
            .collect();
        assert!(
            ids.iter().position(|id| *id == "a").unwrap()
                < ids.iter().position(|id| *id == "z").unwrap(),
            "identical scores retain the original ID tie break"
        );
    }
}
