-- Replace the exact legacy mirror tables after the staged copy and row-count
-- assertions succeed. The caller holds the versioned IMMEDIATE transaction.
DROP TABLE IF EXISTS session_messages;
DROP TABLE IF EXISTS sessions;
ALTER TABLE sessions_scope_new RENAME TO sessions;
ALTER TABLE session_messages_scope_new RENAME TO session_messages;
