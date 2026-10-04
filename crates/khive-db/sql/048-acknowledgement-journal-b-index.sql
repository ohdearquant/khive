-- A replay of migration 045 may restore its older index on the current table.
DROP INDEX IF EXISTS idx_comm_ack_pending;
CREATE INDEX IF NOT EXISTS idx_comm_ack_due
    ON comm_ack_work(state,created_at,delivery_attempt_id,not_before);
