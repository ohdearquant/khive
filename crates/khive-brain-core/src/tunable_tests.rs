use super::*;

#[test]
fn parameter_def_prior_returns_matching_beta_posterior() {
    let def = ParameterDef {
        name: "recall::relevance_weight".into(),
        prior_alpha: 2.0,
        prior_beta: 8.0,
        bounds: (0.0, 1.0),
    };
    let prior = def.prior();
    assert!((prior.alpha() - 2.0).abs() < 1e-12);
    assert!((prior.beta() - 8.0).abs() < 1e-12);
    assert!((prior.mean() - 0.2).abs() < 1e-12);
}

#[test]
fn parameter_space_serializes() {
    let space = ParameterSpace {
        parameters: vec![ParameterDef {
            name: "p".into(),
            prior_alpha: 1.0,
            prior_beta: 1.0,
            bounds: (0.0, 1.0),
        }],
    };
    let json = serde_json::to_string(&space).unwrap();
    let back: ParameterSpace = serde_json::from_str(&json).unwrap();
    assert_eq!(back.parameters.len(), 1);
    assert_eq!(back.parameters[0].name, "p");
}
