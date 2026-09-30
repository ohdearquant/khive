-- Recipient replay is independent of mutable/deletable message history.
-- A stored identity names its message note; a quarantined one has none.
CREATE TABLE IF NOT EXISTS comm_recipient_replay (
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    recipient_agent_id TEXT NOT NULL,
    recipient_actor TEXT NOT NULL,
    note_id TEXT,
    disposition TEXT NOT NULL CHECK(disposition IN ('stored','quarantined')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(sender_agent_id,logical_message_id),
    CHECK((disposition='stored')=(note_id IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS comm_ack_work (
    delivery_attempt_id TEXT PRIMARY KEY,
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    binding TEXT NOT NULL CHECK(json_valid(binding)),
    disposition TEXT NOT NULL CHECK(disposition IN ('stored','quarantined')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','acknowledged')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_comm_ack_pending ON comm_ack_work(state,created_at);
-- Only a policy-refused item keeps its parsed plaintext (ADR-105 A.8).
CREATE TABLE IF NOT EXISTS comm_recipient_quarantine (
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    recipient_agent_id TEXT NOT NULL,
    delivery_item BLOB NOT NULL CHECK(length(delivery_item)<=98304),
    reason TEXT NOT NULL CHECK(reason IN ('invalid_plaintext','invalid_message','policy_rejected')),
    parsed_plaintext TEXT CHECK(parsed_plaintext IS NULL OR json_valid(parsed_plaintext)),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(sender_agent_id,logical_message_id),
    CHECK((reason='policy_rejected')=(parsed_plaintext IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_comm_recipient_quarantine_bound
    ON comm_recipient_quarantine(recipient_agent_id,reason,sender_agent_id,created_at);
