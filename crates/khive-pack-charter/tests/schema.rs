use std::any::Any;

use serde_json::json;

use khive_pack_charter::lang::{compile, Registry};
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

// Earlier charter table definitions used by the upgrade test.
const OLD_SCHEMA: &[&str] = &[
    r###"CREATE TABLE IF NOT EXISTS charter_definitions (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    charter_id TEXT NOT NULL CHECK (length(charter_id) > 0),
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version > 0),
    namespace TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    template_id TEXT NOT NULL CHECK (length(template_id) > 0),
    definition_digest TEXT NOT NULL CHECK (length(definition_digest) > 0),
    definition_bytes BLOB NOT NULL CHECK (typeof(definition_bytes) = 'blob'),
    gate_registry_digest TEXT NOT NULL CHECK (length(gate_registry_digest) > 0),
    action_contract_version TEXT NOT NULL CHECK (length(action_contract_version) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (policy_domain, charter_id, version),
    UNIQUE (policy_domain, charter_id, version, definition_digest)
);

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_replace
BEFORE INSERT ON charter_definitions
WHEN EXISTS (SELECT 1 FROM charter_definitions
    WHERE policy_domain = NEW.policy_domain AND charter_id = NEW.charter_id AND version = NEW.version)
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_update
BEFORE UPDATE ON charter_definitions
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_delete
BEFORE DELETE ON charter_definitions
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_subjects (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    subject_id TEXT NOT NULL CHECK (length(subject_id) > 0),
    namespace TEXT NOT NULL,
    forge TEXT NOT NULL CHECK (length(forge) > 0),
    repository_id TEXT NOT NULL CHECK (length(repository_id) > 0),
    pull_request_id TEXT NOT NULL CHECK (length(pull_request_id) > 0),
    target TEXT NOT NULL CHECK (length(target) > 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    subject_epoch INTEGER NOT NULL DEFAULT 0 CHECK (typeof(subject_epoch) = 'integer' AND subject_epoch >= 0),
    candidate_digest TEXT NOT NULL CHECK (length(candidate_digest) > 0),
    candidate_bytes BLOB NOT NULL CHECK (typeof(candidate_bytes) = 'blob'),
    eligible_run_id TEXT,
    in_flight_attempt_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us >= created_at_us),
    PRIMARY KEY (subject_id),
    UNIQUE (forge, repository_id, pull_request_id, target),
    FOREIGN KEY (subject_id, eligible_run_id)
        REFERENCES charter_runs (subject_id, run_id)
        DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (subject_id, in_flight_attempt_id)
        REFERENCES charter_attempts (subject_id, attempt_id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER IF NOT EXISTS charter_subjects_no_replace
BEFORE INSERT ON charter_subjects
WHEN EXISTS (SELECT 1 FROM charter_subjects
    WHERE subject_id = NEW.subject_id
        OR (forge = NEW.forge AND repository_id = NEW.repository_id
            AND pull_request_id = NEW.pull_request_id AND target = NEW.target))
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_subjects_identity_immutable
BEFORE UPDATE ON charter_subjects
WHEN NEW.subject_id IS NOT OLD.subject_id OR NEW.forge IS NOT OLD.forge
    OR NEW.repository_id IS NOT OLD.repository_id OR NEW.pull_request_id IS NOT OLD.pull_request_id
    OR NEW.target IS NOT OLD.target OR NEW.policy_domain IS NOT OLD.policy_domain
    OR NEW.namespace IS NOT OLD.namespace OR NEW.created_at_us IS NOT OLD.created_at_us
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_subjects_no_delete
BEFORE DELETE ON charter_subjects
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_runs (
    policy_domain TEXT NOT NULL,
    run_id TEXT NOT NULL CHECK (length(run_id) > 0),
    namespace TEXT NOT NULL,
    run_key TEXT NOT NULL UNIQUE CHECK (length(run_key) > 0),
    subject_id TEXT NOT NULL,
    subject_epoch INTEGER NOT NULL CHECK (typeof(subject_epoch) = 'integer' AND subject_epoch >= 0),
    charter_id TEXT NOT NULL,
    definition_version INTEGER NOT NULL,
    definition_digest TEXT NOT NULL,
    candidate_digest TEXT NOT NULL CHECK (length(candidate_digest) > 0),
    candidate_bytes BLOB NOT NULL CHECK (typeof(candidate_bytes) = 'blob'),
    assurance TEXT NOT NULL DEFAULT 'recording_only' CHECK (assurance = 'recording_only'),
    state TEXT NOT NULL CHECK (state IN ('open', 'completed', 'cancelled', 'superseded', 'invalidated', 'failed')),
    current_phase_id TEXT,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us >= created_at_us),
    PRIMARY KEY (policy_domain, run_id),
    UNIQUE (run_id),
    UNIQUE (subject_id, run_id),
    UNIQUE (policy_domain, subject_id, run_id),
    FOREIGN KEY (subject_id)
        REFERENCES charter_subjects (subject_id),
    FOREIGN KEY (policy_domain, charter_id, definition_version, definition_digest)
        REFERENCES charter_definitions (policy_domain, charter_id, version, definition_digest),
    FOREIGN KEY (policy_domain, run_id, current_phase_id)
        REFERENCES charter_phases (policy_domain, run_id, phase_id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER IF NOT EXISTS charter_runs_no_replace
BEFORE INSERT ON charter_runs
WHEN EXISTS (SELECT 1 FROM charter_runs WHERE run_id = NEW.run_id OR run_key = NEW.run_key)
BEGIN
    SELECT RAISE(ABORT, 'charter run identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_runs_pins_immutable
BEFORE UPDATE ON charter_runs
WHEN NEW.policy_domain IS NOT OLD.policy_domain OR NEW.run_id IS NOT OLD.run_id
    OR NEW.namespace IS NOT OLD.namespace OR NEW.run_key IS NOT OLD.run_key
    OR NEW.subject_id IS NOT OLD.subject_id OR NEW.subject_epoch IS NOT OLD.subject_epoch
    OR NEW.charter_id IS NOT OLD.charter_id OR NEW.definition_version IS NOT OLD.definition_version
    OR NEW.definition_digest IS NOT OLD.definition_digest OR NEW.candidate_digest IS NOT OLD.candidate_digest
    OR NEW.candidate_bytes IS NOT OLD.candidate_bytes OR NEW.assurance IS NOT OLD.assurance
    OR NEW.created_at_us IS NOT OLD.created_at_us
BEGIN
    SELECT RAISE(ABORT, 'charter run pins are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_runs_no_delete
BEFORE DELETE ON charter_runs
BEGIN
    SELECT RAISE(ABORT, 'charter run identity is immutable');
END;
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_phases (
    policy_domain TEXT NOT NULL,
    run_id TEXT NOT NULL,
    phase_id TEXT NOT NULL CHECK (length(phase_id) > 0),
    namespace TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (typeof(ordinal) = 'integer' AND ordinal > 0),
    state TEXT NOT NULL CHECK (state IN ('dormant', 'waiting_trigger', 'waiting_gate', 'waiting_actor', 'ready', 'executing', 'uncertain', 'completed')),
    assignee_kind TEXT NOT NULL CHECK (assignee_kind IN ('actor', 'role')),
    assignee TEXT NOT NULL CHECK (length(assignee) > 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us >= created_at_us),
    PRIMARY KEY (policy_domain, run_id, phase_id),
    UNIQUE (policy_domain, run_id, ordinal),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id)
);
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_evidence (
    policy_domain TEXT NOT NULL,
    run_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (typeof(sequence) = 'integer' AND sequence > 0),
    namespace TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    producer TEXT NOT NULL CHECK (length(producer) > 0),
    source_identity TEXT NOT NULL CHECK (length(source_identity) > 0),
    source_event_id TEXT NOT NULL CHECK (length(source_event_id) > 0),
    observed_from_us INTEGER,
    observed_until_us INTEGER,
    completeness TEXT CHECK (completeness IN ('complete', 'partial', 'unavailable')),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) > 0),
    payload_bytes BLOB NOT NULL CHECK (typeof(payload_bytes) = 'blob'),
    supersedes_sequence INTEGER CHECK (supersedes_sequence > 0 AND supersedes_sequence < sequence),
    received_at_us INTEGER NOT NULL CHECK (received_at_us >= 0),
    PRIMARY KEY (policy_domain, run_id, sequence),
    UNIQUE (policy_domain, run_id, producer, kind, source_identity, source_event_id),
    CHECK ((observed_from_us IS NULL AND observed_until_us IS NULL)
        OR (observed_from_us IS NOT NULL AND observed_until_us IS NOT NULL
            AND observed_from_us >= 0 AND observed_until_us >= observed_from_us)),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id),
    FOREIGN KEY (policy_domain, run_id, supersedes_sequence)
        REFERENCES charter_evidence (policy_domain, run_id, sequence)
);

CREATE TRIGGER IF NOT EXISTS charter_evidence_no_replace
BEFORE INSERT ON charter_evidence
WHEN EXISTS (SELECT 1 FROM charter_evidence
    WHERE policy_domain = NEW.policy_domain AND run_id = NEW.run_id
        AND (sequence = NEW.sequence
            OR (producer = NEW.producer AND kind = NEW.kind
                AND source_identity = NEW.source_identity AND source_event_id = NEW.source_event_id)))
BEGIN
    SELECT RAISE(ABORT, 'charter evidence is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_evidence_dense_sequence
BEFORE INSERT ON charter_evidence
WHEN NEW.sequence != COALESCE((SELECT MAX(sequence) FROM charter_evidence
    WHERE policy_domain = NEW.policy_domain AND run_id = NEW.run_id), 0) + 1
BEGIN
    SELECT RAISE(ABORT, 'charter evidence sequence must be dense');
END;

CREATE TRIGGER IF NOT EXISTS charter_evidence_no_update
BEFORE UPDATE ON charter_evidence
BEGIN
    SELECT RAISE(ABORT, 'charter evidence is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_evidence_no_delete
BEFORE DELETE ON charter_evidence
BEGIN
    SELECT RAISE(ABORT, 'charter evidence is immutable');
END;
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_attempts (
    policy_domain TEXT NOT NULL,
    attempt_id TEXT NOT NULL CHECK (length(attempt_id) > 0),
    namespace TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    phase_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('claimed', 'dispatching', 'uncertain', 'resolved')),
    descriptor_digest TEXT NOT NULL CHECK (length(descriptor_digest) > 0),
    descriptor_bytes BLOB NOT NULL CHECK (typeof(descriptor_bytes) = 'blob'),
    evaluation_sequence INTEGER NOT NULL,
    grant_consumption_reference TEXT,
    executor TEXT NOT NULL CHECK (length(executor) > 0),
    deadline_at_us INTEGER NOT NULL CHECK (deadline_at_us >= 0),
    git_receipt_reference TEXT,
    outcome_bytes BLOB CHECK (outcome_bytes IS NULL OR typeof(outcome_bytes) = 'blob'),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    resolved_at_us INTEGER,
    PRIMARY KEY (policy_domain, attempt_id),
    UNIQUE (attempt_id),
    UNIQUE (subject_id, attempt_id),
    CHECK ((state = 'resolved' AND outcome_bytes IS NOT NULL AND resolved_at_us IS NOT NULL
            AND resolved_at_us >= created_at_us)
        OR (state != 'resolved' AND outcome_bytes IS NULL AND resolved_at_us IS NULL)),
    FOREIGN KEY (policy_domain, subject_id, run_id)
        REFERENCES charter_runs (policy_domain, subject_id, run_id),
    FOREIGN KEY (policy_domain, run_id, phase_id)
        REFERENCES charter_phases (policy_domain, run_id, phase_id),
    FOREIGN KEY (policy_domain, run_id, evaluation_sequence)
        REFERENCES charter_evidence (policy_domain, run_id, sequence)
);

CREATE UNIQUE INDEX IF NOT EXISTS charter_attempts_one_unresolved_subject
    ON charter_attempts (subject_id)
    WHERE state IN ('claimed', 'dispatching', 'uncertain');

CREATE TRIGGER IF NOT EXISTS charter_attempts_no_replace
BEFORE INSERT ON charter_attempts
WHEN EXISTS (SELECT 1 FROM charter_attempts
    WHERE attempt_id = NEW.attempt_id
        OR (subject_id = NEW.subject_id AND state IN ('claimed', 'dispatching', 'uncertain')
            AND NEW.state IN ('claimed', 'dispatching', 'uncertain')))
BEGIN
    SELECT RAISE(ABORT, 'charter attempt identity and unresolved slot cannot be replaced');
END;

CREATE TRIGGER IF NOT EXISTS charter_attempts_pins_immutable
BEFORE UPDATE ON charter_attempts
WHEN NEW.policy_domain IS NOT OLD.policy_domain OR NEW.attempt_id IS NOT OLD.attempt_id
    OR NEW.namespace IS NOT OLD.namespace OR NEW.subject_id IS NOT OLD.subject_id
    OR NEW.run_id IS NOT OLD.run_id OR NEW.phase_id IS NOT OLD.phase_id
    OR NEW.descriptor_digest IS NOT OLD.descriptor_digest OR NEW.descriptor_bytes IS NOT OLD.descriptor_bytes
    OR NEW.evaluation_sequence IS NOT OLD.evaluation_sequence
    OR NEW.grant_consumption_reference IS NOT OLD.grant_consumption_reference
    OR NEW.executor IS NOT OLD.executor OR NEW.deadline_at_us IS NOT OLD.deadline_at_us
    OR NEW.created_at_us IS NOT OLD.created_at_us
BEGIN
    SELECT RAISE(ABORT, 'charter attempt pins are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_attempts_no_delete
BEFORE DELETE ON charter_attempts
BEGIN
    SELECT RAISE(ABORT, 'charter attempt history is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_attempts_slot_no_replace
BEFORE UPDATE ON charter_attempts
WHEN NEW.state IN ('claimed', 'dispatching', 'uncertain')
    AND EXISTS (SELECT 1 FROM charter_attempts
        WHERE subject_id = NEW.subject_id AND attempt_id != OLD.attempt_id
            AND state IN ('claimed', 'dispatching', 'uncertain'))
BEGIN
    SELECT RAISE(ABORT, 'charter unresolved attempt slot cannot be replaced');
END;
"###,
    r###"CREATE TABLE IF NOT EXISTS charter_commands (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    caller TEXT NOT NULL CHECK (length(caller) > 0),
    request_id TEXT NOT NULL CHECK (length(request_id) > 0),
    namespace TEXT NOT NULL,
    verb TEXT NOT NULL CHECK (length(verb) > 0),
    request_digest TEXT NOT NULL CHECK (length(request_digest) > 0),
    disposition TEXT NOT NULL CHECK (length(disposition) > 0),
    result_bytes BLOB NOT NULL CHECK (typeof(result_bytes) = 'blob'),
    run_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (caller, request_id),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id)
);

CREATE TRIGGER IF NOT EXISTS charter_commands_no_replace
BEFORE INSERT ON charter_commands
WHEN EXISTS (SELECT 1 FROM charter_commands WHERE caller = NEW.caller AND request_id = NEW.request_id)
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_commands_no_update
BEFORE UPDATE ON charter_commands
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_commands_no_delete
BEFORE DELETE ON charter_commands
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;
"###,
];

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
async fn inventory_loading_installs_nine_tables_and_reapplying_preserves_rows() {
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
            "charter_vocabularies",
            "charter_waiting_changes",
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
        "INSERT INTO charter_subjects (policy_domain, subject_id, namespace, subject_kind, subject_ref, candidate_digest, candidate_bytes, created_at_us, updated_at_us) VALUES (?1, ?2, 'test', 'pull_request', ?3, 'candidate-a', X'7B7D', 1, 1)",
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
        "INSERT INTO charter_phases (policy_domain, run_id, phase_id, namespace, ordinal, state, assignee_kind, assignee, round, created_at_us, updated_at_us) VALUES (?1, ?2, ?3, 'test', ?4, 'waiting_gate', 'role', 'reviewer', 0, 1, 1)",
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

include!("schema_tests/identity_and_replay_admission_tests.rs");

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
async fn main_schema_upgrades_in_place_and_keeps_a_waiting_run_readable() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec![CharterPack::NAME.to_owned()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated runtime");
    runtime
        .backend()
        .apply_pack_ddl_statements(OLD_SCHEMA)
        .expect("install the previous charter schema");

    let source = "charter upgrade_wait v1\nsubject pull_request\nworkflow:\n    phase gate:\n        require pull_request.open hard\n        complete\n";
    let compiled = compile(source, &Registry::core()).expect("valid waiting charter");
    let mut writer = runtime.sql().writer().await.expect("writer");
    writer
        .execute(sql(
            "INSERT INTO charter_definitions (policy_domain, charter_id, version, namespace, schema_version, template_id, definition_digest, definition_bytes, gate_registry_digest, action_contract_version, created_at_us) VALUES ('default', 'upgrade_wait', 1, 'local', 1, 'charter/1', ?1, ?2, ?3, 'recording/1', 1)",
            vec![
                text(&compiled.definition_digest),
                SqlValue::Blob(compiled.definition_bytes.clone()),
                text(&compiled.gate_registry_digest),
            ],
        ))
        .await
        .expect("insert the pinned definition");
    writer
        .execute(sql(
            "INSERT INTO charter_subjects (policy_domain, subject_id, namespace, forge, repository_id, pull_request_id, target, candidate_digest, candidate_bytes, created_at_us, updated_at_us) VALUES ('default', 'subject-old', 'local', 'github', 'acme/widgets', '7', 'main', 'candidate-old', X'7B7D', 1, 1)",
            vec![],
        ))
        .await
        .expect("insert the old subject row");
    writer
        .execute(sql(
            "INSERT INTO charter_runs (policy_domain, run_id, namespace, run_key, subject_id, subject_epoch, charter_id, definition_version, definition_digest, candidate_digest, candidate_bytes, state, current_phase_id, created_at_us, updated_at_us) VALUES ('default', 'run-old', 'local', 'old-run-key', 'subject-old', 0, 'upgrade_wait', 1, ?1, 'candidate-old', X'7B2267656E65726174696F6E223A6E756C6C7D', 'open', NULL, 1, 1)",
            vec![text(&compiled.definition_digest)],
        ))
        .await
        .expect("insert the waiting run");
    writer
        .execute(sql(
            "INSERT INTO charter_phases (policy_domain, run_id, phase_id, namespace, ordinal, state, assignee_kind, assignee, created_at_us, updated_at_us) VALUES ('default', 'run-old', 'gate', 'local', 1, 'waiting_gate', 'role', 'reviewer', 1, 1)",
            vec![],
        ))
        .await
        .expect("insert the waiting phase");
    writer
        .execute(sql(
            "UPDATE charter_runs SET current_phase_id = 'gate' WHERE run_id = 'run-old'",
            vec![],
        ))
        .await
        .expect("select the waiting phase");
    drop(writer);

    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &[CharterPack::NAME.to_owned()],
        runtime.clone(),
        &mut builder,
    )
    .expect("register charter pack");
    let registry = builder.build().expect("charter registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("upgrade the old schema transactionally");

    let run = registry
        .dispatch("charter.get", json!({"run_id": "run-old"}))
        .await
        .expect("read the retained run");
    assert_eq!(run["run_id"], "run-old");
    assert_eq!(run["state"], "open");
    assert_eq!(run["current_phase"], "gate");
    assert_eq!(run["phases"][0]["state"], "waiting_gate");
    assert_eq!(run["phases"][0]["round"], 1);
    assert_eq!(run["subject"]["kind"], "pull_request");
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

include!("schema_tests/scoped_reference_and_assurance_tests.rs");
