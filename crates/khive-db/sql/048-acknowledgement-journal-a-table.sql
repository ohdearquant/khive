-- Retry scheduling belongs to the durable journal, not the node driver's memory.
CREATE TABLE comm_ack_work_journal (
    delivery_attempt_id TEXT PRIMARY KEY,
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    binding TEXT NOT NULL CHECK(json_valid(binding)),
    disposition TEXT NOT NULL CHECK(disposition IN ('stored','quarantined')),
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK(state IN ('pending','acknowledged','retired')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(attempt_count)='integer' AND attempt_count>=0),
    not_before INTEGER,
    retirement_reason TEXT
        CHECK(retirement_reason IS NULL OR retirement_reason='permanent_transport'),
    CHECK((state='retired')=(retirement_reason IS NOT NULL))
);

INSERT INTO comm_ack_work_journal
    (delivery_attempt_id,sender_agent_id,logical_message_id,binding,disposition,
     state,created_at,updated_at)
SELECT delivery_attempt_id,sender_agent_id,logical_message_id,binding,disposition,
       state,created_at,updated_at
FROM comm_ack_work;

DROP TABLE comm_ack_work;
ALTER TABLE comm_ack_work_journal RENAME TO comm_ack_work;
