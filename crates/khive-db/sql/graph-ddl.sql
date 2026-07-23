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

-- Single-origin invariant on concept `introduced_by` edges — mirrors
-- migration 013 for callers that create this table via ensure_graph_schema()
-- without ever running the versioned migration chain (e.g. StorageBackend::graph()
-- callers that never call run_migrations()).
CREATE TRIGGER IF NOT EXISTS trg_graph_edges_concept_single_origin_insert
BEFORE INSERT ON graph_edges
WHEN NEW.relation = 'introduced_by'
 AND NEW.deleted_at IS NULL
 AND EXISTS (
     SELECT 1 FROM entities
     WHERE id = NEW.source_id
       AND kind = 'concept'
       AND deleted_at IS NULL
 )
 AND EXISTS (
     SELECT 1 FROM graph_edges
     WHERE namespace = NEW.namespace
       AND source_id = NEW.source_id
       AND relation = 'introduced_by'
       AND target_id <> NEW.target_id
       AND deleted_at IS NULL
       AND id <> NEW.id
 )
BEGIN
    SELECT RAISE(ABORT, 'concept already has a different introduced_by origin');
END;

CREATE TRIGGER IF NOT EXISTS trg_graph_edges_concept_single_origin_update
BEFORE UPDATE OF namespace, source_id, target_id, relation, deleted_at ON graph_edges
WHEN NEW.relation = 'introduced_by'
 AND NEW.deleted_at IS NULL
 AND EXISTS (
     SELECT 1 FROM entities
     WHERE id = NEW.source_id
       AND kind = 'concept'
       AND deleted_at IS NULL
 )
 AND EXISTS (
     SELECT 1 FROM graph_edges
     WHERE namespace = NEW.namespace
       AND source_id = NEW.source_id
       AND relation = 'introduced_by'
       AND target_id <> NEW.target_id
       AND deleted_at IS NULL
       AND (namespace <> OLD.namespace OR id <> OLD.id)
 )
BEGIN
    SELECT RAISE(ABORT, 'concept already has a different introduced_by origin');
END;
