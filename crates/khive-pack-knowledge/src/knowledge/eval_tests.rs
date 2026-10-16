use super::*;

#[test]
fn expected_set_metrics_use_fixed_k_and_ranked_first_hit() {
    let expected = HashSet::from(["alpha".to_string(), "beta".to_string()]);
    let returned = vec!["noise".to_string(), "beta".to_string(), "alpha".to_string()];
    let metrics = score_query(&expected, &returned);
    assert_eq!(metrics.precision_at_5, 0.4);
    assert_eq!(metrics.recall_at_5, 1.0);
    assert_eq!(metrics.mrr, 0.5);
}

#[test]
fn results_beyond_fixed_k_do_not_count() {
    let expected = HashSet::from(["expected".to_string()]);
    let returned = ["a", "b", "c", "d", "e", "expected"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let metrics = score_query(&expected, &returned);
    assert_eq!(metrics.precision_at_5, 0.0);
    assert_eq!(metrics.recall_at_5, 0.0);
    assert_eq!(metrics.mrr, 0.0);
}

#[test]
fn query_set_validation_names_the_invalid_entry() {
    let error = parse_query_set(
        Path::new("fixture.toml"),
        r#"
                [[queries]]
                query = "valid"
                expected_slugs = ["alpha"]

                [[queries]]
                query = " "
                expected_slugs = ["beta"]
            "#,
    )
    .expect_err("blank query must fail");
    let message = error.to_string();
    assert!(message.contains("fixture.toml"));
    assert!(message.contains("[1]"));
}

#[test]
fn empty_query_set_is_a_whole_file_validation_error() {
    for contents in ["", "# no queries yet\n"] {
        let error = parse_query_set(Path::new("fixture.toml"), contents)
            .expect_err("query set without entries must fail");
        let message = error.to_string();
        assert!(message.contains("fixture.toml"));
        assert!(message.contains("contains no queries"));
        assert!(!message.contains("invalid TOML syntax"));
    }
}

#[test]
fn shipped_query_set_is_valid_and_publicly_bounded() {
    let queries = parse_query_set(
        Path::new("eval_set.toml"),
        include_str!("../../tests/fixtures/eval_set.toml"),
    )
    .expect("shipped query set must validate");
    assert_eq!(queries.len(), 10);
}
