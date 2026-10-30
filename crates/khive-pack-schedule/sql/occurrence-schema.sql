-- Durable occurrence accounting owned by the schedule pack.
CREATE TABLE IF NOT EXISTS scheduled_definitions_v3 (
    store_id TEXT NOT NULL,
    schedule_id TEXT NOT NULL,
    definition_revision INTEGER NOT NULL CHECK (definition_revision > 0),
    namespace TEXT NOT NULL,
    definition_digest BLOB NOT NULL CHECK (length(definition_digest) = 32),
    canonical_definition BLOB NOT NULL,
    created_at_us INTEGER NOT NULL,
    PRIMARY KEY (store_id, schedule_id, definition_revision)
);
CREATE TABLE IF NOT EXISTS scheduled_series_v3 (
    store_id TEXT NOT NULL,
    schedule_id TEXT NOT NULL,
    definition_revision INTEGER NOT NULL,
    namespace TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('active','suspended','cancelled','exhausted')),
    next_seconds INTEGER,
    next_nanos INTEGER CHECK (next_nanos BETWEEN 0 AND 999999999),
    window_source_seconds INTEGER,
    window_source_nanos INTEGER CHECK (window_source_nanos BETWEEN 0 AND 999999999),
    horizon_seconds INTEGER,
    horizon_nanos INTEGER CHECK (horizon_nanos BETWEEN 0 AND 999999999),
    selected_seconds INTEGER,
    selected_nanos INTEGER CHECK (selected_nanos BETWEEN 0 AND 999999999),
    selection_at_us INTEGER,
    effective_max_lateness_s INTEGER CHECK (effective_max_lateness_s >= 0),
    active_occurrence_id TEXT,
    active_invocation_id TEXT,
    suspended_invocation_id TEXT,
    cancellation_cut_us INTEGER,
    accounting_blocked_reason TEXT,
    PRIMARY KEY (store_id, schedule_id),
    FOREIGN KEY (store_id, schedule_id, definition_revision)
        REFERENCES scheduled_definitions_v3(store_id, schedule_id, definition_revision),
    CHECK ((next_seconds IS NULL) = (next_nanos IS NULL)),
    CHECK ((horizon_seconds IS NULL) = (horizon_nanos IS NULL)),
    CHECK ((selected_seconds IS NULL) = (selected_nanos IS NULL)),
    CHECK ((window_source_seconds IS NULL) = (window_source_nanos IS NULL)),
    CHECK ((active_occurrence_id IS NULL) = (active_invocation_id IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_scheduled_series_v3_due
    ON scheduled_series_v3(namespace, state, next_seconds, next_nanos, schedule_id);
CREATE TABLE IF NOT EXISTS scheduled_occurrences_v3 (
    id TEXT PRIMARY KEY,
    store_id TEXT NOT NULL,
    schedule_id TEXT NOT NULL,
    definition_revision INTEGER NOT NULL,
    namespace TEXT NOT NULL,
    scheduled_seconds INTEGER NOT NULL,
    scheduled_nanos INTEGER NOT NULL CHECK (scheduled_nanos BETWEEN 0 AND 999999999),
    materialized_at_us INTEGER NOT NULL,
    selection_at_us INTEGER NOT NULL,
    effective_max_lateness_s INTEGER NOT NULL CHECK (effective_max_lateness_s >= 0),
    UNIQUE (store_id, schedule_id, definition_revision, scheduled_seconds, scheduled_nanos),
    FOREIGN KEY (store_id, schedule_id, definition_revision)
        REFERENCES scheduled_definitions_v3(store_id, schedule_id, definition_revision)
);
CREATE INDEX IF NOT EXISTS idx_scheduled_occurrences_v3_page
    ON scheduled_occurrences_v3(namespace, schedule_id, definition_revision,
        scheduled_seconds, scheduled_nanos, id);
CREATE TABLE IF NOT EXISTS scheduled_attempts_v3 (
    id TEXT PRIMARY KEY,
    occurrence_id TEXT NOT NULL UNIQUE REFERENCES scheduled_occurrences_v3(id),
    namespace TEXT NOT NULL,
    invocation_id TEXT NOT NULL UNIQUE,
    claim_generation INTEGER NOT NULL CHECK (claim_generation > 0),
    owner_nonce TEXT NOT NULL,
    lease_expires_at_us INTEGER NOT NULL,
    launch_intent_id TEXT UNIQUE,
    admission_at_us INTEGER,
    launch_intent_at_us INTEGER,
    CHECK ((launch_intent_id IS NULL) = (launch_intent_at_us IS NULL)),
    CHECK ((launch_intent_id IS NULL) = (admission_at_us IS NULL))
);
CREATE TABLE IF NOT EXISTS scheduled_receipts_v3 (
    id TEXT PRIMARY KEY,
    occurrence_id TEXT NOT NULL REFERENCES scheduled_occurrences_v3(id),
    namespace TEXT NOT NULL,
    record_kind TEXT NOT NULL CHECK (record_kind IN ('terminal','reconciliation')),
    outcome TEXT NOT NULL CHECK (outcome IN
        ('succeeded','failed','timed_out','held','missed','refused',
         'spawn_failed','cancelled','indeterminate')),
    reason_code TEXT NOT NULL,
    invocation_id TEXT,
    exec_receipt_id TEXT,
    prior_receipt_id TEXT REFERENCES scheduled_receipts_v3(id),
    recorded_at_us INTEGER NOT NULL,
    final_decision_at_us INTEGER,
    started_at_us INTEGER,
    completed_at_us INTEGER,
    exit_code INTEGER,
    exit_signal INTEGER,
    canonical_evidence BLOB NOT NULL,
    CHECK ((record_kind = 'terminal' AND prior_receipt_id IS NULL) OR
        (record_kind = 'reconciliation' AND prior_receipt_id IS NOT NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_scheduled_receipts_v3_terminal
    ON scheduled_receipts_v3(occurrence_id) WHERE record_kind = 'terminal';
CREATE INDEX IF NOT EXISTS idx_scheduled_receipts_v3_history
    ON scheduled_receipts_v3(namespace, occurrence_id, recorded_at_us, id);
CREATE TABLE IF NOT EXISTS scheduled_notice_outbox_v3 (
    id TEXT PRIMARY KEY,
    store_id TEXT NOT NULL,
    occurrence_id TEXT NOT NULL REFERENCES scheduled_occurrences_v3(id),
    terminal_receipt_id TEXT NOT NULL REFERENCES scheduled_receipts_v3(id),
    namespace TEXT NOT NULL,
    recipient_kind TEXT NOT NULL,
    recipient_id TEXT NOT NULL,
    notice_kind TEXT NOT NULL CHECK (notice_kind = 'outcome_notice'),
    intent_digest BLOB NOT NULL CHECK (length(intent_digest) = 32),
    canonical_intent BLOB NOT NULL,
    created_at_us INTEGER NOT NULL,
    UNIQUE (store_id, occurrence_id, recipient_kind, recipient_id, notice_kind)
);
CREATE INDEX IF NOT EXISTS idx_scheduled_notice_outbox_v3_page
    ON scheduled_notice_outbox_v3(namespace, created_at_us, id);
CREATE INDEX IF NOT EXISTS idx_scheduled_notice_outbox_v3_work
    ON scheduled_notice_outbox_v3(store_id, created_at_us, id);
CREATE TRIGGER IF NOT EXISTS scheduled_definitions_v3_immutable_update
    BEFORE UPDATE ON scheduled_definitions_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_definitions_v3_immutable_delete
    BEFORE DELETE ON scheduled_definitions_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_occurrences_v3_immutable_update
    BEFORE UPDATE ON scheduled_occurrences_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_occurrences_v3_immutable_delete
    BEFORE DELETE ON scheduled_occurrences_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_receipts_v3_immutable_update
    BEFORE UPDATE ON scheduled_receipts_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_receipts_v3_immutable_delete
    BEFORE DELETE ON scheduled_receipts_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_notice_outbox_v3_immutable_update
    BEFORE UPDATE ON scheduled_notice_outbox_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_notice_outbox_v3_immutable_delete
    BEFORE DELETE ON scheduled_notice_outbox_v3
    BEGIN SELECT RAISE(ABORT, 'immutable scheduled occurrence record'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_attempts_v3_launch_write_once
    BEFORE UPDATE ON scheduled_attempts_v3
    WHEN OLD.launch_intent_id IS NOT NULL AND
        (NEW.launch_intent_id IS NOT OLD.launch_intent_id OR
         NEW.launch_intent_at_us IS NOT OLD.launch_intent_at_us OR
         NEW.admission_at_us IS NOT OLD.admission_at_us OR
         NEW.occurrence_id IS NOT OLD.occurrence_id OR
         NEW.invocation_id IS NOT OLD.invocation_id)
    BEGIN SELECT RAISE(ABORT, 'scheduled launch intent is immutable'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_series_v3_preserve_owner
    BEFORE UPDATE ON scheduled_series_v3
    WHEN NEW.store_id IS NOT OLD.store_id OR NEW.schedule_id IS NOT OLD.schedule_id
        OR NEW.namespace IS NOT OLD.namespace
        OR (NEW.definition_revision IS NOT OLD.definition_revision AND
            (OLD.active_invocation_id IS NOT NULL OR OLD.suspended_invocation_id IS NOT NULL))
    BEGIN SELECT RAISE(ABORT, 'scheduled series ownership or unsettled launch changed'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_series_v3_no_delete
    BEFORE DELETE ON scheduled_series_v3
    BEGIN SELECT RAISE(ABORT, 'scheduled series accounting cannot be deleted'); END;
-- Active schedule notes remain attached until explicit cancellation.
CREATE TRIGGER IF NOT EXISTS scheduled_series_v3_note_move_delete_guard
    BEFORE UPDATE OF namespace, deleted_at ON notes
    WHEN (NEW.namespace IS NOT OLD.namespace OR NEW.deleted_at IS NOT OLD.deleted_at)
        AND EXISTS (SELECT 1 FROM scheduled_series_v3
            WHERE schedule_id = OLD.id AND state IN ('active','suspended'))
    BEGIN SELECT RAISE(ABORT, 'cancel active version-3 schedule before moving or deleting note'); END;
CREATE TRIGGER IF NOT EXISTS scheduled_series_v3_note_hard_delete_guard
    BEFORE DELETE ON notes
    WHEN EXISTS (SELECT 1 FROM scheduled_series_v3
        WHERE schedule_id = OLD.id AND state IN ('active','suspended'))
    BEGIN SELECT RAISE(ABORT, 'cancel active version-3 schedule before deleting note'); END;
