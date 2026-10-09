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
