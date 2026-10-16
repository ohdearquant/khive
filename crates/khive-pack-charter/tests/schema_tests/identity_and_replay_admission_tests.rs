#[tokio::test]
async fn definition_identity_is_versioned_and_body_is_immutable() {
    let f = Fixture::new();
    assert_eq!(
        f.execute(definition("policy-a", "merge", 1)).await.unwrap(),
        1
    );
    f.refuse(definition("policy-a", "merge", 1), "immutable")
        .await;
    for statement in [
        sql(
            "UPDATE charter_definitions SET definition_bytes = X'7B2278223A317D'",
            vec![],
        ),
        sql("DELETE FROM charter_definitions", vec![]),
        SqlStatement {
            sql: definition("policy-a", "merge", 1).sql.replacen(
                "INSERT INTO",
                "INSERT OR REPLACE INTO",
                1,
            ),
            ..definition("policy-a", "merge", 1)
        },
    ] {
        f.refuse(statement, "immutable").await;
    }
    for statement in [
        definition("policy-a", "merge", 2),
        definition("policy-b", "merge", 1),
        definition("policy-a", "deploy", 1),
    ] {
        assert_eq!(f.execute(statement).await.unwrap(), 1);
    }
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_definitions").await, 4);
}

#[tokio::test]
async fn logical_subject_identity_cannot_be_duplicated_by_changing_policy_domain() {
    let f = Fixture::new();
    assert_eq!(
        f.execute(subject("policy-a", "subject-a", "pr-1"))
            .await
            .unwrap(),
        1
    );
    f.refuse(
        subject("policy-a", "another-id", "pr-1"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    f.refuse(
        subject("policy-b", "another-id", "pr-1"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    f.refuse(
        subject("policy-b", "subject-a", "pr-2"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    assert_eq!(
        f.execute(subject("policy-b", "subject-b", "pr-2"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_subjects").await, 2);
}

#[tokio::test]
async fn run_replay_key_is_unique_across_definition_versions() {
    let f = Fixture::new();
    f.seed().await;
    assert_eq!(
        f.execute(definition("policy-a", "merge", 2)).await.unwrap(),
        1
    );
    f.refuse(
        run("policy-a", "run-b", "subject-a", "key-a", "merge", 2),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    assert_eq!(
        f.execute(run("policy-a", "run-b", "subject-a", "key-b", "merge", 2))
            .await
            .unwrap(),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_runs").await, 2);
}

#[tokio::test]
async fn caller_request_replay_key_cannot_be_reused_for_another_verb() {
    let f = Fixture::new();
    assert_eq!(
        f.execute(command(
            "policy-a",
            CALLER_A,
            "request-1",
            "charter.trigger"
        ))
        .await
        .unwrap(),
        1
    );
    f.refuse(
        command("policy-a", CALLER_A, "request-1", "charter.advance"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    f.refuse(
        command("policy-b", CALLER_A, "request-1", "charter.advance"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    assert_eq!(
        f.execute(command(
            "policy-a",
            CALLER_B,
            "request-1",
            "charter.advance"
        ))
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        f.execute(command(
            "policy-a",
            CALLER_A,
            "request-2",
            "charter.advance"
        ))
        .await
        .unwrap(),
        1
    );
    for statement in [
        sql(
            "UPDATE charter_commands SET request_digest = 'different'",
            vec![],
        ),
        sql("DELETE FROM charter_commands", vec![]),
    ] {
        f.refuse(statement, "immutable").await;
    }
    let replacement = command("policy-b", CALLER_A, "request-1", "charter.advance");
    f.refuse(
        SqlStatement {
            sql: replacement
                .sql
                .replacen("INSERT INTO", "INSERT OR REPLACE INTO", 1),
            ..replacement
        },
        "immutable",
    )
    .await;
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_commands").await, 3);
}
