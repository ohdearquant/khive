-- V25: add message direction to the unread-probe recipient index key.
--
-- V22 keyed the partial index on (namespace, kind, recipient, created_at,
-- id), but the comm unread-count and unread-listing queries also filter on
-- direction='inbound' as a residual predicate evaluated after the index
-- seek (a bound parameter, not part of the index). Every comm.send call
-- durably writes an outbound copy addressed to the recipient
-- (khive-pack-comm dual_write_message) whose `read` property starts at
-- `false` and is never subsequently marked read, because the recipient only
-- ever reads their inbound copy. That outbound copy therefore sits in the
-- same partial-index partition as the recipient's unread inbound rows for
-- the rest of the note's lifetime, so a recipient with a small unread inbox
-- but a long outbound history pays for scanning that whole history on every
-- unread probe: the LIMIT cap in the bounded-count query bounds rows that
-- match the full WHERE clause, not index entries visited before the
-- direction filter rejects them.
--
-- Adding direction as a key column keeps the recipient-scoped seek and lets
-- the planner exclude outbound copies inside the index using the same bound
-- equality parameter the direction filter already binds — a key column
-- needs no plan-time literal, unlike the read-status predicate in the
-- partial index's own WHERE clause.

DROP INDEX IF EXISTS idx_notes_unread_probe_recipient;

CREATE INDEX IF NOT EXISTS idx_notes_unread_probe_recipient_direction
    ON notes(namespace, kind,
             ifnull(json_extract(properties, '$.to_actor'), ''),
             json_extract(properties, '$.direction'),
             created_at DESC, id ASC)
    WHERE (json_type(properties, '$.read') IS NULL
           OR json_type(properties, '$.read') != 'true')
      AND deleted_at IS NULL;
