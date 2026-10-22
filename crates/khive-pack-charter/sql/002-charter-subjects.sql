CREATE TABLE IF NOT EXISTS charter_subjects (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    subject_id TEXT NOT NULL CHECK (length(subject_id) > 0),
    namespace TEXT NOT NULL,
    subject_kind TEXT,
    subject_ref TEXT,
    forge TEXT,
    repository_id TEXT,
    pull_request_id TEXT,
    target TEXT,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    subject_epoch INTEGER NOT NULL DEFAULT 0 CHECK (typeof(subject_epoch) = 'integer' AND subject_epoch >= 0),
    candidate_digest TEXT NOT NULL CHECK (length(candidate_digest) > 0),
    candidate_bytes BLOB NOT NULL CHECK (typeof(candidate_bytes) = 'blob'),
    eligible_run_id TEXT,
    in_flight_attempt_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us >= created_at_us),
    PRIMARY KEY (subject_id),
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
        OR (subject_kind = NEW.subject_kind AND subject_ref = NEW.subject_ref))
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_subjects_identity_immutable
BEFORE UPDATE ON charter_subjects
WHEN NEW.subject_id IS NOT OLD.subject_id OR NEW.subject_kind IS NOT OLD.subject_kind
    OR NEW.subject_ref IS NOT OLD.subject_ref OR NEW.policy_domain IS NOT OLD.policy_domain
    OR NEW.namespace IS NOT OLD.namespace OR NEW.created_at_us IS NOT OLD.created_at_us
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_subjects_no_delete
BEFORE DELETE ON charter_subjects
BEGIN
    SELECT RAISE(ABORT, 'charter subject identity is immutable');
END;

CREATE UNIQUE INDEX IF NOT EXISTS charter_subjects_kind_ref_unique
    ON charter_subjects (subject_kind, subject_ref);

CREATE TRIGGER IF NOT EXISTS charter_subjects_require_current_identity
BEFORE INSERT ON charter_subjects
WHEN NEW.subject_kind IS NULL OR length(NEW.subject_kind) = 0
    OR NEW.subject_ref IS NULL OR length(NEW.subject_ref) = 0
BEGIN
    SELECT RAISE(ABORT, 'charter subject kind and reference are required');
END;

UPDATE charter_subjects
SET subject_kind = 'pull_request',
    subject_ref = 'legacy:' || hex(CAST(forge AS BLOB)) || ':' || hex(CAST(repository_id AS BLOB))
        || ':' || hex(CAST(pull_request_id AS BLOB)) || ':' || hex(CAST(target AS BLOB))
WHERE subject_kind IS NULL OR subject_ref IS NULL;

CREATE TRIGGER IF NOT EXISTS charter_subjects_identity_immutable_v2
BEFORE UPDATE ON charter_subjects
WHEN NEW.subject_kind IS NOT OLD.subject_kind OR NEW.subject_ref IS NOT OLD.subject_ref
BEGIN
    SELECT RAISE(ABORT, 'charter subject kind and reference are immutable');
END;
