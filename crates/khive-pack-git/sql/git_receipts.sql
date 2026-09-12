CREATE TABLE IF NOT EXISTS git_receipts (
    id TEXT PRIMARY KEY NOT NULL,
    namespace TEXT NOT NULL,
    actor TEXT NOT NULL,
    session_id TEXT,
    verb TEXT NOT NULL,
    repo TEXT NOT NULL,
    inputs TEXT NOT NULL CHECK (json_valid(inputs)),
    gate TEXT NOT NULL CHECK (json_valid(gate)),
    policy TEXT CHECK (policy IS NULL OR json_valid(policy)),
    fork_policy TEXT CHECK (fork_policy IS NULL OR json_valid(fork_policy)),
    credential TEXT CHECK (credential IS NULL OR json_valid(credential)),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    disposition TEXT NOT NULL CHECK (disposition IN ('unknown', 'committed', 'not_committed')),
    result TEXT NOT NULL CHECK (json_valid(result)),
    reason TEXT,
    CHECK (disposition = 'unknown' OR finished_at IS NOT NULL)
);
