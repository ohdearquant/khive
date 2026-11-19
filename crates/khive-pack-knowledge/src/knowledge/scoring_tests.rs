use super::*;

#[test]
fn coverage_counts_only_own_expansion_for_each_original_term() {
    let originals = vec!["alpha".to_owned(), "beta".to_owned()];
    let mut terms = originals.clone();
    expand_terms(&mut terms);
    let idf = terms.iter().map(|term| (term.clone(), 1.0)).collect();
    let weights = Weights::default();
    let unpenalized = Weights {
        coverage_alpha: 0.0,
        ..Weights::default()
    };
    let candidate = |content: &str| Candidate {
        id: "test".into(),
        slug: "test".into(),
        name_raw: String::new(),
        content_raw: Some(content.into()),
        tags_raw: None,
        atom_embed_text: None,
        status_raw: None,
        finalized: false,
        is_domain: false,
        name: Vec::new(),
        tags: Vec::new(),
        content: matching::tokenize_field(content),
    };
    let score = |content: &str, weights: &Weights| {
        score_candidate(
            &candidate(content),
            &terms,
            &originals,
            &originals,
            &idf,
            "alpha beta",
            weights,
        )
    };

    let complete = score("alpha beta", &weights);
    let complete_base = score("alpha beta", &unpenalized);
    assert!((complete - complete_base).abs() < 1e-6);

    let missing = score("alpha", &weights);
    let missing_base = score("alpha", &unpenalized);
    assert!((missing / missing_base - 0.5_f32.sqrt()).abs() < 1e-6);

    let expanded = score("alpha betas", &weights);
    let expanded_base = score("alpha betas", &unpenalized);
    assert!((expanded - expanded_base).abs() < 1e-6);
}
