use std::any::Any;

use khive_pack_charter::transition::{
    transition_phase_in_transaction, transition_run_in_transaction, PhaseState, PhaseTransition,
    RunState, RunTransition,
};
use khive_pack_charter::CharterPack;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use khive_storage::{
    SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult, WriterTaskRequestState,
};
use khive_types::Pack;

const CALLER_A: &str = r#"{"kind":"agent","id":"caller-a"}"#;
const CALLER_B: &str = r#"{"kind":"agent","id":"caller-b"}"#;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            packs: vec![CharterPack::NAME.to_owned()],
            credentials: Vec::new(),
            mounts: Vec::new(),
            events_split: None,
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            visibility_receipts: None,
            blob: Default::default(),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("isolated runtime without embedding services");
        assert!(!runtime.backend().is_file_backed());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs(
            &[CharterPack::NAME.to_owned()],
            runtime.clone(),
            &mut builder,
        )
        .expect("charter is discovered through its inventory factory");
        let registry = builder.build().expect("charter registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("pack schema installation");
        Self { runtime, registry }
    }

    async fn execute(&self, statement: SqlStatement) -> StorageResult<u64> {
        self.runtime.sql().writer().await?.execute(statement).await
    }

    async fn count(&self, query: &str) -> u64 {
        self.runtime
            .sql()
            .reader()
            .await
            .expect("reader")
            .count(sql(query, vec![]))
            .await
            .expect("count")
    }
}

fn sql(query: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement::new(query, params)
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

fn committed() -> Box<dyn Any + Send> {
    Box::new(())
}

fn assert_revision_conflict(error: &StorageError) {
    let StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source,
    } = error
    else {
        panic!("expected a rolled-back writer request, got {error:?}");
    };
    match source.as_ref() {
        StorageError::Conflict {
            capability: StorageCapability::Sql,
            operation,
            message,
        } => {
            assert_eq!(operation, "charter.transition");
            assert!(message.contains("RevisionConflict"), "{message}");
        }
        other => panic!("expected SQL transition conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn inventory_loading_installs_seven_tables_and_reapplying_preserves_rows() {
    let fixture = Fixture::new();
    let mut reader = fixture.runtime.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(sql(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name LIKE 'charter_%' ORDER BY name",
            vec![],
        ))
        .await
        .expect("charter tables");
    let names: Vec<_> = rows.iter().map(|row| row.text("name").unwrap()).collect();
    assert_eq!(
        names,
        [
            "charter_attempts",
            "charter_commands",
            "charter_definitions",
            "charter_evidence",
            "charter_phases",
            "charter_runs",
            "charter_subjects",
        ]
    );
    drop(reader);
    fixture.seed().await;
    fixture
        .registry
        .apply_schema_plans_with_map(&Default::default(), fixture.runtime.backend())
        .expect("idempotent schema installation");
    assert_eq!(fixture.count("SELECT COUNT(*) FROM charter_runs").await, 1);
}

fn definition(domain: &str, charter: &str, version: i64) -> SqlStatement {
    sql(
        "INSERT INTO charter_definitions (policy_domain, charter_id, version, namespace, schema_version, template_id, definition_digest, definition_bytes, gate_registry_digest, action_contract_version, created_at_us) VALUES (?1, ?2, ?3, 'test', 1, 'pr_merge/v1', ?4, X'7B7D', 'registry-v1', 'git_pr_merge/v1', 1)",
        vec![text(domain), text(charter), SqlValue::Integer(version), text(&format!("{charter}-{version}"))],
    )
}

fn subject(domain: &str, id: &str, pull_request: &str) -> SqlStatement {
    sql(
        "INSERT INTO charter_subjects (policy_domain, subject_id, namespace, forge, repository_id, pull_request_id, target, candidate_digest, candidate_bytes, created_at_us, updated_at_us) VALUES (?1, ?2, 'test', 'github', 'repository-1', ?3, 'main', 'candidate-a', X'7B7D', 1, 1)",
        vec![text(domain), text(id), text(pull_request)],
    )
}

fn run(
    domain: &str,
    id: &str,
    subject: &str,
    key: &str,
    charter: &str,
    version: i64,
) -> SqlStatement {
    sql(
        "INSERT INTO charter_runs (policy_domain, run_id, namespace, run_key, subject_id, subject_epoch, charter_id, definition_version, definition_digest, candidate_digest, candidate_bytes, state, created_at_us, updated_at_us) VALUES (?1, ?2, 'test', ?3, ?4, 0, ?5, ?6, ?7, 'candidate-a', X'7B7D', 'open', 1, 1)",
        vec![text(domain), text(id), text(key), text(subject), text(charter), SqlValue::Integer(version), text(&format!("{charter}-{version}"))],
    )
}

fn phase(domain: &str, run: &str, id: &str, ordinal: i64) -> SqlStatement {
    sql(
        "INSERT INTO charter_phases (policy_domain, run_id, phase_id, namespace, ordinal, state, assignee_kind, assignee, created_at_us, updated_at_us) VALUES (?1, ?2, ?3, 'test', ?4, 'waiting_gate', 'role', 'reviewer', 1, 1)",
        vec![text(domain), text(run), text(id), SqlValue::Integer(ordinal)],
    )
}

fn evidence(domain: &str, run: &str, sequence: i64) -> SqlStatement {
    sql(
        "INSERT INTO charter_evidence (policy_domain, run_id, sequence, namespace, kind, producer, source_identity, source_event_id, observed_from_us, observed_until_us, completeness, payload_digest, payload_bytes, received_at_us) VALUES (?1, ?2, ?3, 'test', 'evaluation', 'test-producer', 'test-source', ?4, 1, 1, 'complete', ?5, X'7B7D', 1)",
        vec![text(domain), text(run), SqlValue::Integer(sequence), text(&format!("event-{sequence}")), text(&format!("payload-{sequence}"))],
    )
}

fn command(domain: &str, caller: &str, request: &str, verb: &str) -> SqlStatement {
    sql(
        "INSERT INTO charter_commands (policy_domain, caller, request_id, namespace, verb, request_digest, disposition, result_bytes, created_at_us) VALUES (?1, ?2, ?3, 'test', ?4, ?5, 'recorded', X'7B7D', 1)",
        vec![text(domain), text(caller), text(request), text(verb), text(&format!("digest-{verb}"))],
    )
}

fn attempt(domain: &str, id: &str, subject: &str, run: &str, state: &str) -> SqlStatement {
    sql(
        "INSERT INTO charter_attempts (policy_domain, attempt_id, namespace, subject_id, run_id, phase_id, state, descriptor_digest, descriptor_bytes, evaluation_sequence, executor, deadline_at_us, created_at_us) VALUES (?1, ?2, 'test', ?3, ?4, 'validate', ?5, 'descriptor-a', X'7B7D', 1, 'executor', 100, 1)",
        vec![text(domain), text(id), text(subject), text(run), text(state)],
    )
}

impl Fixture {
    async fn seed(&self) {
        for statement in [
            definition("policy-a", "merge", 1),
            subject("policy-a", "subject-a", "pr-1"),
            run("policy-a", "run-a", "subject-a", "key-a", "merge", 1),
            phase("policy-a", "run-a", "validate", 1),
            sql("UPDATE charter_runs SET current_phase_id = 'validate' WHERE policy_domain = 'policy-a' AND run_id = 'run-a'", vec![]),
        ] {
            assert_eq!(self.execute(statement).await.unwrap(), 1);
        }
    }

    async fn refuse(&self, statement: SqlStatement, expected: &str) {
        let error = self
            .execute(statement)
            .await
            .expect_err("constraint must refuse");
        assert!(
            expected
                .split('|')
                .any(|reason| error.to_string().contains(reason)),
            "expected {expected:?}, got {error}"
        );
    }

    async fn run_row(&self) -> (String, i64) {
        let row = self.runtime.sql().reader().await.unwrap()
            .query_row(sql("SELECT state, revision FROM charter_runs WHERE policy_domain = 'policy-a' AND run_id = 'run-a'", vec![]))
            .await.unwrap().expect("seeded run");
        (
            row.text("state").unwrap().to_owned(),
            row.i64("revision").unwrap(),
        )
    }
}

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

#[tokio::test]
async fn evidence_is_dense_per_run_and_cannot_be_rewritten_or_deleted() {
    let f = Fixture::new();
    f.seed().await;
    f.refuse(evidence("policy-a", "run-a", 2), "dense").await;
    assert_eq!(
        f.execute(evidence("policy-a", "run-a", 1)).await.unwrap(),
        1
    );
    f.runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("PRAGMA recursive_triggers = OFF".to_owned())
        .await
        .unwrap();
    let mut replaced_source = evidence("policy-a", "run-a", 2);
    replaced_source.sql = replaced_source
        .sql
        .replacen("INSERT INTO", "INSERT OR REPLACE INTO", 1);
    replaced_source.params[3] = text("event-1");
    f.refuse(replaced_source, "immutable").await;
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_evidence WHERE sequence = 1 AND payload_digest = 'payload-1' AND hex(payload_bytes) = '7B7D'").await, 1);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_evidence WHERE sequence > 1")
            .await,
        0
    );
    f.refuse(evidence("policy-a", "run-a", 3), "dense").await;
    f.refuse(evidence("policy-a", "run-a", 1), "dense|immutable")
        .await;
    for statement in [
        sql("UPDATE charter_evidence SET payload_bytes = X'00'", vec![]),
        sql("DELETE FROM charter_evidence", vec![]),
    ] {
        f.refuse(statement, "immutable").await;
    }
    let replace = evidence("policy-a", "run-a", 1);
    f.refuse(
        SqlStatement {
            sql: replace
                .sql
                .replacen("INSERT INTO", "INSERT OR REPLACE INTO", 1),
            ..replace
        },
        "dense|immutable",
    )
    .await;
    assert_eq!(
        f.execute(evidence("policy-a", "run-a", 2)).await.unwrap(),
        1
    );
    assert_eq!(
        f.execute(run("policy-a", "run-b", "subject-a", "key-b", "merge", 1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f.execute(evidence("policy-a", "run-b", 1)).await.unwrap(),
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_evidence WHERE run_id = 'run-a'")
            .await,
        2
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_evidence WHERE run_id = 'run-b'")
            .await,
        1
    );
}

#[tokio::test]
async fn uncertain_attempt_retains_exclusivity_across_definition_versions() {
    let f = Fixture::new();
    f.seed().await;
    for statement in [
        definition("policy-a", "merge", 2),
        run("policy-a", "run-b", "subject-a", "key-b", "merge", 2),
        phase("policy-a", "run-b", "validate", 1),
        evidence("policy-a", "run-a", 1),
        evidence("policy-a", "run-b", 1),
        attempt("policy-a", "attempt-a", "subject-a", "run-a", "uncertain"),
    ] {
        assert_eq!(f.execute(statement).await.unwrap(), 1);
    }
    f.refuse(
        attempt("policy-a", "attempt-b", "subject-a", "run-b", "claimed"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    f.runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("PRAGMA recursive_triggers = OFF".to_owned())
        .await
        .unwrap();
    let replace_slot = attempt("policy-a", "attempt-b", "subject-a", "run-b", "claimed");
    f.refuse(
        SqlStatement {
            sql: replace_slot
                .sql
                .replacen("INSERT INTO", "INSERT OR REPLACE INTO", 1),
            ..replace_slot
        },
        "cannot be replaced",
    )
    .await;
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts WHERE attempt_id = 'attempt-a' AND state = 'uncertain' AND descriptor_digest = 'descriptor-a' AND hex(descriptor_bytes) = '7B7D'").await, 1);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_attempts WHERE attempt_id = 'attempt-b'")
            .await,
        0
    );
    assert_eq!(f.execute(sql(
        "INSERT INTO charter_attempts (policy_domain, attempt_id, namespace, subject_id, run_id, phase_id, state, descriptor_digest, descriptor_bytes, evaluation_sequence, executor, deadline_at_us, created_at_us, resolved_at_us, outcome_bytes) VALUES ('policy-a', 'attempt-history', 'test', 'subject-a', 'run-a', 'validate', 'resolved', 'descriptor-history', X'7B7D', 1, 'executor', 100, 1, 2, X'7B7D')",
        vec![],
    )).await.unwrap(), 1);
    f.refuse(sql(
        "UPDATE OR REPLACE charter_attempts SET state = 'claimed', resolved_at_us = NULL, outcome_bytes = NULL WHERE attempt_id = 'attempt-history'",
        vec![],
    ), "cannot be replaced").await;
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts WHERE attempt_id = 'attempt-history' AND state = 'resolved' AND descriptor_digest = 'descriptor-history'").await, 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts WHERE attempt_id = 'attempt-a' AND state = 'uncertain'").await, 1);
    for statement in [
        definition("policy-b", "merge", 1),
        run("policy-b", "run-d", "subject-a", "key-d", "merge", 1),
        phase("policy-b", "run-d", "validate", 1),
        evidence("policy-b", "run-d", 1),
    ] {
        assert_eq!(f.execute(statement).await.unwrap(), 1);
    }
    f.refuse(
        attempt("policy-b", "attempt-d", "subject-a", "run-d", "claimed"),
        "UNIQUE|immutable|cannot be replaced",
    )
    .await;
    for statement in [
        subject("policy-a", "subject-b", "pr-2"),
        run("policy-a", "run-c", "subject-b", "key-c", "merge", 1),
        phase("policy-a", "run-c", "validate", 1),
        evidence("policy-a", "run-c", 1),
        attempt("policy-a", "attempt-c", "subject-b", "run-c", "claimed"),
    ] {
        assert_eq!(f.execute(statement).await.unwrap(), 1);
    }
    assert_eq!(f.execute(sql("UPDATE charter_attempts SET state = 'resolved', outcome_bytes = X'7B7D', resolved_at_us = 2 WHERE attempt_id = 'attempt-a'", vec![])).await.unwrap(), 1);
    assert_eq!(
        f.execute(attempt(
            "policy-a",
            "attempt-b",
            "subject-a",
            "run-b",
            "claimed"
        ))
        .await
        .unwrap(),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts").await, 4);
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts WHERE attempt_id = 'attempt-a' AND state = 'resolved'").await, 1);
}

impl Fixture {
    async fn transition_run(
        &self,
        domain: &'static str,
        expected_revision: i64,
        expected_state: RunState,
        next_state: RunState,
    ) -> StorageResult<i64> {
        let result = self
            .runtime
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    let revision = transition_run_in_transaction(
                        writer,
                        RunTransition {
                            policy_domain: domain,
                            run_id: "run-a",
                            expected_revision,
                            expected_state,
                            next_state,
                            at_us: 2,
                        },
                    )
                    .await
                    .map_err(StorageError::from)?;
                    Ok(Box::new(revision) as Box<dyn Any + Send>)
                })
            }))
            .await?;
        Ok(*result.downcast::<i64>().expect("revision result"))
    }

    async fn transition_phase(
        &self,
        domain: &'static str,
        expected_revision: i64,
        expected_state: PhaseState,
    ) -> StorageResult<i64> {
        let result = self
            .runtime
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    let revision = transition_phase_in_transaction(
                        writer,
                        PhaseTransition {
                            policy_domain: domain,
                            run_id: "run-a",
                            phase_id: "validate",
                            expected_revision,
                            expected_state,
                            next_state: PhaseState::Ready,
                            at_us: 2,
                        },
                    )
                    .await
                    .map_err(StorageError::from)?;
                    Ok(Box::new(revision) as Box<dyn Any + Send>)
                })
            }))
            .await?;
        Ok(*result.downcast::<i64>().expect("revision result"))
    }
}

#[tokio::test]
async fn run_transition_requires_matching_revision_state_and_policy_domain() {
    let f = Fixture::new();
    f.seed().await;
    assert_eq!(
        f.transition_run("policy-a", 0, RunState::Open, RunState::Completed)
            .await
            .unwrap(),
        1
    );
    for (domain, revision, state) in [
        ("policy-a", 0, RunState::Completed),
        ("policy-a", 1, RunState::Open),
        ("policy-b", 1, RunState::Completed),
    ] {
        let error = f
            .transition_run(domain, revision, state, RunState::Failed)
            .await
            .unwrap_err();
        assert_revision_conflict(&error);
        assert_eq!(f.run_row().await, ("completed".to_owned(), 1));
    }
}

#[tokio::test]
async fn phase_transition_requires_matching_revision_state_and_policy_domain() {
    let f = Fixture::new();
    f.seed().await;
    assert_eq!(
        f.transition_phase("policy-a", 0, PhaseState::WaitingGate)
            .await
            .unwrap(),
        1
    );
    for (domain, revision, state) in [
        ("policy-a", 0, PhaseState::Ready),
        ("policy-a", 1, PhaseState::WaitingGate),
        ("policy-b", 1, PhaseState::Ready),
    ] {
        let error = f
            .transition_phase(domain, revision, state)
            .await
            .unwrap_err();
        assert_revision_conflict(&error);
        assert_eq!(f.count("SELECT COUNT(*) FROM charter_phases WHERE phase_id = 'validate' AND state = 'ready' AND revision = 1").await, 1);
    }
}

#[tokio::test]
async fn invalid_or_exhausted_revision_is_refused_without_writing() {
    let f = Fixture::new();
    f.seed().await;
    for revision in [-1, i64::MAX] {
        let result = f
            .transition_run("policy-a", revision, RunState::Open, RunState::Completed)
            .await;
        let error = result.expect_err("invalid revision must refuse");
        assert!(error.to_string().contains("InvalidRevision"), "{error}");
        assert_eq!(f.run_row().await, ("open".to_owned(), 0));
    }
}

#[tokio::test]
async fn cas_loss_rolls_back_preceding_evidence_and_command_then_valid_retry_commits() {
    let f = Fixture::new();
    f.seed().await;
    for (expected_revision, should_commit) in [(1, false), (0, true)] {
        let result = f
            .runtime
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer.execute(evidence("policy-a", "run-a", 1)).await?;
                    writer
                        .execute(command(
                            "policy-a",
                            CALLER_A,
                            "request-1",
                            "charter.advance",
                        ))
                        .await?;
                    transition_run_in_transaction(
                        writer,
                        RunTransition {
                            policy_domain: "policy-a",
                            run_id: "run-a",
                            expected_revision,
                            expected_state: RunState::Open,
                            next_state: RunState::Completed,
                            at_us: 2,
                        },
                    )
                    .await
                    .map_err(StorageError::from)?;
                    Ok(committed())
                })
            }))
            .await;
        if should_commit {
            assert!(result.is_ok(), "matching CAS and its records must commit");
            assert_eq!(f.run_row().await, ("completed".to_owned(), 1));
        } else {
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("stale CAS must fail"),
            };
            assert_revision_conflict(&error);
            assert_eq!(f.run_row().await, ("open".to_owned(), 0));
        }
        let expected = u64::from(should_commit);
        assert_eq!(
            f.count("SELECT COUNT(*) FROM charter_evidence").await,
            expected
        );
        assert_eq!(
            f.count("SELECT COUNT(*) FROM charter_commands").await,
            expected
        );
    }
}

#[tokio::test]
async fn later_command_failure_rolls_back_prior_cas_and_evidence() {
    let f = Fixture::new();
    f.seed().await;
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
    let result = f
        .runtime
        .sql()
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                transition_run_in_transaction(
                    writer,
                    RunTransition {
                        policy_domain: "policy-a",
                        run_id: "run-a",
                        expected_revision: 0,
                        expected_state: RunState::Open,
                        next_state: RunState::Completed,
                        at_us: 2,
                    },
                )
                .await
                .map_err(StorageError::from)?;
                writer.execute(evidence("policy-a", "run-a", 1)).await?;
                writer
                    .execute(command(
                        "policy-a",
                        CALLER_A,
                        "request-1",
                        "charter.advance",
                    ))
                    .await?;
                Ok(committed())
            })
        }))
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("later replay collision must fail"),
    };
    assert!(
        error.to_string().contains("immutable"),
        "must reach the later command collision: {error}"
    );
    assert_eq!(f.run_row().await, ("open".to_owned(), 0));
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_evidence").await, 0);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_commands WHERE verb = 'charter.trigger'")
            .await,
        1
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM charter_commands WHERE verb = 'charter.advance'")
            .await,
        0
    );
}

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
    assert!(CharterPack::HANDLERS.is_empty());
    assert!(f.registry.describe_verb("charter.claim").is_err());
    assert!(f
        .registry
        .dispatch("charter.claim", serde_json::json!({}))
        .await
        .is_err());
    assert_eq!(f.count("SELECT COUNT(*) FROM charter_attempts").await, 0);
}
