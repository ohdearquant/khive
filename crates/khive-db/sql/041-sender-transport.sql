CREATE TABLE comm_sender_transport (
    namespace TEXT NOT NULL,
    logical_message_id TEXT NOT NULL,
    outbound_note_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    slug TEXT NOT NULL,
    credential_ref TEXT NOT NULL,
    recipient_address TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    sender_agent_id TEXT NOT NULL,
    sender_assurance TEXT NOT NULL
        CHECK(sender_assurance IN ('claimed','daemon_bearer','actor_signature')),
    recipient_agent_id TEXT NOT NULL,
    recipient_device_id TEXT NOT NULL,
    recipient_key_epoch INTEGER NOT NULL,
    contact_generation INTEGER NOT NULL,
    sender_key_epoch INTEGER NOT NULL,
    recipient_key_fingerprint TEXT NOT NULL,
    enc BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    envelope_seq INTEGER NOT NULL CHECK(envelope_seq > 0),
    state TEXT NOT NULL CHECK(state IN ('pending','recipient_stored','recipient_quarantined','failed')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    next_retry_at INTEGER,
    last_failure_class TEXT CHECK(last_failure_class IN ('transient','authentication','permanent')),
    hold_reason TEXT CHECK(hold_reason IN ('insufficient_credit','recipient_key_changed','policy_denied')),
    policy_mode TEXT CHECK(policy_mode IN ('off','shadow','enforce')),
    policy_revision INTEGER CHECK(policy_revision >= 0),
    receipt TEXT CHECK(receipt IS NULL OR json_valid(receipt)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK((hold_reason IS 'policy_denied' AND policy_mode IS NOT NULL
           AND policy_revision IS NOT NULL)
       OR (hold_reason IS NOT 'policy_denied' AND policy_mode IS NULL
           AND policy_revision IS NULL)),
    CHECK(state = 'pending' OR hold_reason IS NULL),
    PRIMARY KEY(logical_message_id, recipient_device_id, recipient_key_epoch),
    UNIQUE(logical_message_id,envelope_seq),
    CHECK(length(enc) = 32), CHECK(length(ciphertext) <= 65536),
    CHECK(recipient_key_epoch BETWEEN 1 AND 4294967295),
    CHECK(sender_key_epoch BETWEEN 1 AND 4294967295),
    CHECK(contact_generation BETWEEN 1 AND 4294967295)
);
CREATE INDEX idx_comm_sender_pending_route ON comm_sender_transport(namespace,kind,slug,state,next_retry_at);
