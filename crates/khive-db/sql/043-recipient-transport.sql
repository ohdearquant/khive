-- Recipient replay is independent of mutable/deletable message history.
CREATE TABLE comm_recipient_replay (
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    recipient_agent_id TEXT NOT NULL,
    recipient_actor TEXT NOT NULL,
    note_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('stored','quarantined')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(sender_agent_id,logical_message_id)
);
CREATE TABLE comm_ack_work (
    delivery_attempt_id TEXT PRIMARY KEY,
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    binding TEXT NOT NULL CHECK(json_valid(binding)),
    disposition TEXT NOT NULL CHECK(disposition IN ('stored','quarantined')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','acknowledged')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_comm_ack_pending ON comm_ack_work(state,created_at);
CREATE TABLE comm_recipient_quarantine (
    sender_agent_id TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    delivery_item BLOB NOT NULL CHECK(length(delivery_item)<=98304),
    reason TEXT NOT NULL CHECK(reason IN ('invalid_plaintext','invalid_message','policy_rejected')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(sender_agent_id,logical_message_id)
);
