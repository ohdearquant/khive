#[tokio::test]
async fn scoped_references_cannot_attach_phases_or_attempts_to_another_run() {
    let f = Fixture::new();
    f.seed().await;
    assert_eq!(
        f.execute(subject("policy-a", "subject-b", "pr-2"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f.execute(evidence("policy-a", "run-a", 1)).await.unwrap(),
        1
    );
    f.refuse(
        run(
            "policy-a",
            "run-missing-definition",
            "subject-a",
            "key-missing",
            "merge",
            2,
        ),
        "FOREIGN KEY",
    )
    .await;
    f.refuse(phase("policy-b", "run-a", "validate", 1), "FOREIGN KEY")
        .await;
    f.refuse(
        attempt(
            "policy-a",
            "attempt-wrong-subject",
            "subject-b",
            "run-a",
            "claimed",
        ),
        "FOREIGN KEY",
    )
    .await;
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_runs").await, 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_phases").await, 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts").await, 0);
    assert_eq!(
        f.execute(attempt(
            "policy-a",
            "attempt-valid",
            "subject-a",
            "run-a",
            "claimed"
        ))
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn foundation_records_only_recording_assurance_and_exposes_no_claim() {
    let f = Fixture::new();
    f.seed().await;
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_runs WHERE assurance = 'recording_only'")
            .await,
        1
    );
    f.refuse(sql(
        "INSERT INTO charter_runs (policy_domain, run_id, namespace, run_key, subject_id, subject_epoch, charter_id, definition_version, definition_digest, candidate_digest, candidate_bytes, assurance, state, created_at_us, updated_at_us) SELECT policy_domain, 'run-enforced', namespace, 'key-enforced', subject_id, subject_epoch, charter_id, definition_version, definition_digest, candidate_digest, candidate_bytes, 'enforced', state, created_at_us, updated_at_us FROM charter_runs WHERE run_id = 'run-a'",
        vec![],
    ), "CHECK").await;
    assert!(CharterPack::HANDLERS
        .iter()
        .all(|h| h.name != "charter.claim"));
    assert!(f.registry.describe_verb("charter.claim").is_err());
    assert!(f
        .registry
        .dispatch("charter.claim", serde_json::json!({}))
        .await
        .is_err());
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts").await, 0);
}
