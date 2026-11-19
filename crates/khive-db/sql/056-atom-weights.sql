CREATE TABLE IF NOT EXISTS atom_weights (
    namespace  TEXT NOT NULL,
    atom_id    TEXT NOT NULL,
    weight     REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (namespace, atom_id)
);
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
