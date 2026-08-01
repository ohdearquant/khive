-- Graph edges table and supporting indexes.
-- Applied idempotently by StorageBackend::graph_for_namespace on every store access.

CREATE TABLE IF NOT EXISTS graph_edges (
    namespace      TEXT NOT NULL,
    id             TEXT NOT NULL,
    source_id      TEXT NOT NULL,
    target_id      TEXT NOT NULL,
    relation       TEXT NOT NULL,
    weight         REAL NOT NULL DEFAULT 1.0,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    metadata       TEXT,
    target_backend TEXT,
    PRIMARY KEY (namespace, id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_unique_triple ON graph_edges(namespace, source_id, target_id, relation);
CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_source ON graph_edges(namespace, source_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_target ON graph_edges(namespace, target_id);
CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_relation ON graph_edges(namespace, relation);
CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_src_rel ON graph_edges(namespace, source_id, relation);
CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_tgt_rel ON graph_edges(namespace, target_id, relation);
CREATE INDEX IF NOT EXISTS idx_graph_edges_target_backend ON graph_edges(target_backend) WHERE target_backend IS NOT NULL;

-- Durable list-cursor insertion order. This mirrors migration V13 as a
-- belt-and-suspenders path for fresh/direct store construction that applies
-- GRAPH_DDL without running the core migration chain. Existing databases are
-- backfilled only by V13, in operator context.
CREATE TABLE IF NOT EXISTS graph_edges_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id TEXT NOT NULL UNIQUE
);

CREATE TRIGGER IF NOT EXISTS assign_graph_edge_list_seq
AFTER INSERT ON graph_edges
BEGIN
    -- Explicit UPSERT keeps the ledger immutable even when an outer caller
    -- uses `INSERT OR REPLACE` (which overrides legacy trigger OR policies).
    INSERT INTO graph_edges_seq (edge_id) VALUES (NEW.id)
    ON CONFLICT(edge_id) DO NOTHING;
END;
