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
