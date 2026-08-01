-- V13: durable insertion sequences for stable entity/note/edge list cursors
-- (khive #1424, #1462).
--
-- Wall-clock timestamps plus random UUIDs are a total order, but not an
-- insertion order: a later commit can share the cursor's microsecond and carry
-- a lower UUID. These AUTOINCREMENT ledgers are assigned by AFTER INSERT
-- triggers in the same SQLite transaction as the substrate insert. Values are
-- immutable, never reused after hard deletion, and unaffected by VACUUM.

CREATE TABLE IF NOT EXISTS entities_seq (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS graph_edges_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id TEXT NOT NULL UNIQUE
);

-- Pre-V13 `graph_edges` only enforced `PRIMARY KEY (namespace, id)`, so a
-- legacy database can hold two edges sharing an id across namespaces. The
-- backfill below keys the ledger by edge id alone (`INSERT OR IGNORE`), so
-- such a pair would silently collapse onto one shared sequence row instead
-- of failing. Enforcing global id uniqueness here, before that backfill
-- runs, means a legacy duplicate fails this migration's own transaction --
-- so V13 rolls back in full (no ledger tables, no triggers) rather than
-- committing an ambiguous ledger for a later migration to discover. See
-- 014-graph-edges-id-unique.sql, which re-asserts the same index
-- (IF NOT EXISTS, so idempotent here) for any database that already
-- committed this V13 migration before this guard existed.
CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_id_unique ON graph_edges(id);

-- Entity/note upgrade backfills use deterministic `(created_at, id)` order.
-- Edge backfill preserves the UUID ordering of the public pre-V13 edge cursor,
-- so an outstanding cursor can resume across the migration without silently
-- skipping or repeating an existing edge. The notes anti-join intentionally
-- repairs any ledger hole as well as reusing the existing V7/V8 `notes_seq`
-- table.
INSERT OR IGNORE INTO entities_seq (entity_id)
SELECT id FROM entities ORDER BY created_at ASC, id ASC;

INSERT OR IGNORE INTO notes_seq (note_id)
SELECT n.id FROM notes n
WHERE NOT EXISTS (SELECT 1 FROM notes_seq s WHERE s.note_id = n.id)
ORDER BY n.created_at ASC, n.id ASC;

INSERT OR IGNORE INTO graph_edges_seq (edge_id)
SELECT id FROM graph_edges ORDER BY id ASC;

CREATE TRIGGER IF NOT EXISTS assign_entity_list_seq
AFTER INSERT ON entities
BEGIN
    -- Explicit UPSERT syntax is load-bearing. An outer `INSERT OR REPLACE`
    -- overrides legacy `OR IGNORE` inside a trigger and would otherwise replace
    -- this ledger row, moving an existing entity to a new sequence.
    INSERT INTO entities_seq (entity_id) VALUES (NEW.id)
    ON CONFLICT(entity_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS assign_note_list_seq
AFTER INSERT ON notes
BEGIN
    INSERT INTO notes_seq (note_id) VALUES (NEW.id)
    ON CONFLICT(note_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS assign_graph_edge_list_seq
AFTER INSERT ON graph_edges
BEGIN
    INSERT INTO graph_edges_seq (edge_id) VALUES (NEW.id)
    ON CONFLICT(edge_id) DO NOTHING;
END;
