/// `max_items` boundary handling (ADR-088 Amendment 1):
/// a negative value must clamp to the lower bound (1), NOT silently fall
/// through `as_u64`'s failure into the 500 default. `0` clamps to 1 too;
/// values above 2000 clamp to 2000; a non-integer value is a hard error.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_max_items_negative_and_zero_clamp_to_one() {
    let _guard = ENV_MUTEX.lock().await;

    for requested in [-1, 0] {
        let (_rt, _token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            write(repo, "f.txt", &format!("v{i}\n"));
            commit(repo, &["f.txt"], &format!("commit {i}"));
        }

        let resp = registry
            .dispatch(
                "git.digest",
                json!({"source": repo.to_str().unwrap(), "max_items": requested, "include": ["commits"]}),
            )
            .await
            .unwrap_or_else(|e| panic!("digest ok for max_items={requested}: {e}"));
        assert_eq!(
            resp["commits_ingested"].as_u64().unwrap(),
            1,
            "max_items={requested} must clamp to the lower bound (1 item this call): {resp:?}"
        );
        assert_eq!(resp["max_items_requested"], requested);
        assert_eq!(resp["max_items_effective"], 1);
        let expected_warning = format!("max_items request {requested} was clamped to 1");
        assert!(
            resp["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning.as_str() == Some(expected_warning.as_str())),
            "the report must disclose the lower-bound clamp: {resp:?}"
        );
        assert!(
            !resp["done"].as_bool().unwrap(),
            "2 commits remain after a 1-item pass: {resp:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_max_items_above_cap_clamps_to_two_thousand() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "f.txt", "v\n");
    commit(repo, &["f.txt"], "only commit");

    let resp = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 2001, "include": ["commits"]}),
        )
        .await
        .expect("digest ok");
    assert_eq!(resp["commits_ingested"].as_u64().unwrap(), 1);
    assert_eq!(resp["max_items_requested"], 2001);
    assert_eq!(resp["max_items_effective"], 2000);
    assert!(
        resp["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str() == Some("max_items request 2001 was clamped to 2000")),
        "the report must disclose the clamp: {resp:?}"
    );
    assert!(
        resp["done"].as_bool().unwrap(),
        "a single-commit repo finishes in one call however large max_items clamps to: {resp:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_max_items_at_boundary_values() {
    let _guard = ENV_MUTEX.lock().await;
    for (requested, expected_ingested) in [(1i64, 1u64), (2000i64, 1u64)] {
        let (_rt, _token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_repo(repo);
        write(repo, "f.txt", "v\n");
        commit(repo, &["f.txt"], "only commit");

        let resp = registry
            .dispatch(
                "git.digest",
                json!({"source": repo.to_str().unwrap(), "max_items": requested, "include": ["commits"]}),
            )
            .await
            .unwrap_or_else(|e| panic!("digest ok for max_items={requested}: {e}"));
        assert_eq!(
            resp["commits_ingested"].as_u64().unwrap(),
            expected_ingested,
            "max_items={requested}: {resp:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_rejects_non_integer_max_items() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "f.txt", "v\n");
    commit(repo, &["f.txt"], "only commit");

    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": "lots"}),
        )
        .await
        .expect_err("a non-integer max_items must be a hard error, not a silent default");
    assert!(
        format!("{err}").contains("max_items"),
        "error must name the offending field: {err}"
    );
}
