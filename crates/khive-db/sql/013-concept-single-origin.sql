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
