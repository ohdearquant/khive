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
