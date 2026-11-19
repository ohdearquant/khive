CREATE TABLE IF NOT EXISTS charter_commands (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    caller TEXT NOT NULL CHECK (length(caller) > 0),
    request_id TEXT NOT NULL CHECK (length(request_id) > 0),
    namespace TEXT NOT NULL,
    verb TEXT NOT NULL CHECK (length(verb) > 0),
    request_digest TEXT NOT NULL CHECK (length(request_digest) > 0),
    disposition TEXT NOT NULL CHECK (length(disposition) > 0),
    result_bytes BLOB NOT NULL CHECK (typeof(result_bytes) = 'blob'),
    run_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (caller, request_id),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id)
);

CREATE TRIGGER IF NOT EXISTS charter_commands_no_replace
BEFORE INSERT ON charter_commands
WHEN EXISTS (SELECT 1 FROM charter_commands WHERE caller = NEW.caller AND request_id = NEW.request_id)
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_commands_no_update
BEFORE UPDATE ON charter_commands
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_commands_no_delete
BEFORE DELETE ON charter_commands
BEGIN
    SELECT RAISE(ABORT, 'charter command receipts are immutable');
END;
