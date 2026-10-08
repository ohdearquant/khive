CREATE TABLE IF NOT EXISTS weight_events (
    namespace    TEXT NOT NULL,
    atom_id      TEXT NOT NULL,
    delta        REAL NOT NULL,
    weight_after REAL NOT NULL,
    channel      TEXT NOT NULL,
    eta          REAL NOT NULL,
    event_id     TEXT,
    context_id   TEXT,
    ts           INTEGER NOT NULL
);
