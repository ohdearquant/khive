CREATE TABLE IF NOT EXISTS atom_weights (
    namespace  TEXT NOT NULL,
    atom_id    TEXT NOT NULL,
    weight     REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (namespace, atom_id)
);
