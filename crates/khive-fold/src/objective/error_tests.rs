use super::*;

#[test]
fn test_no_candidates_display() {
    let err = ObjectiveError::NoCandidates;
    assert_eq!(err.to_string(), "No candidates available");
}

#[test]
fn test_no_match_display() {
    let err = ObjectiveError::NoMatch("score < 0.5".into());
    assert!(err.to_string().contains("score < 0.5"));
}

#[test]
fn test_invalid_config_display() {
    let err = ObjectiveError::InvalidConfig("missing weight".into());
    assert!(err.to_string().contains("missing weight"));
}

#[test]
fn test_not_found_display() {
    let err = ObjectiveError::NotFound("relevance_v1".into());
    assert!(err.to_string().contains("relevance_v1"));
}

#[test]
fn test_scoring_display() {
    let err = ObjectiveError::Scoring("NaN result".into());
    assert!(err.to_string().contains("NaN result"));
}
