CREATE TABLE IF NOT EXISTS charter_definitions (
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

CREATE TABLE IF NOT EXISTS charter_subjects (
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

CREATE TABLE IF NOT EXISTS charter_runs (
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

CREATE TABLE IF NOT EXISTS charter_phases (
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

CREATE TABLE IF NOT EXISTS charter_evidence (
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

CREATE TABLE IF NOT EXISTS charter_attempts (
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

CREATE TABLE IF NOT EXISTS charter_commands (
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
