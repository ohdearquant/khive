CREATE TABLE IF NOT EXISTS operation_slot_decisions (
    namespace TEXT NOT NULL CHECK (length(namespace) > 0),
    decision_id TEXT NOT NULL CHECK (length(decision_id) > 0),
    definition_id TEXT NOT NULL CHECK (length(definition_id) > 0),
    form_id TEXT NOT NULL CHECK (length(form_id) > 0),
    subject_ids TEXT NOT NULL CHECK (json_valid(subject_ids)
        AND json_type(subject_ids) = 'array' AND json_array_length(subject_ids) > 0),
    type_version INTEGER NOT NULL CHECK (typeof(type_version) = 'integer' AND type_version > 0),
    form_revision TEXT NOT NULL CHECK (length(form_revision) > 0),
    slot TEXT CHECK (slot IS NULL OR length(slot) > 0),
    reason TEXT NOT NULL CHECK (length(reason) > 0),
    decided_by TEXT NOT NULL CHECK (length(decided_by) > 0),
    decided_at_us INTEGER NOT NULL CHECK (decided_at_us >= 0),
    supersedes TEXT NOT NULL CHECK (json_valid(supersedes) AND json_type(supersedes) = 'array'),
    PRIMARY KEY (namespace, decision_id)
);

CREATE TRIGGER IF NOT EXISTS operation_slot_decisions_no_replace
BEFORE INSERT ON operation_slot_decisions
WHEN EXISTS (SELECT 1 FROM operation_slot_decisions
    WHERE namespace = NEW.namespace AND decision_id = NEW.decision_id)
BEGIN
    SELECT RAISE(ABORT, 'operation slot decisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_slot_decisions_no_update
BEFORE UPDATE ON operation_slot_decisions
BEGIN
    SELECT RAISE(ABORT, 'operation slot decisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_slot_decisions_no_delete
BEFORE DELETE ON operation_slot_decisions
BEGIN
    SELECT RAISE(ABORT, 'operation slot decisions are immutable');
END;
