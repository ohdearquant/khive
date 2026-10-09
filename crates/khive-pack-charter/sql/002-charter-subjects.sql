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
