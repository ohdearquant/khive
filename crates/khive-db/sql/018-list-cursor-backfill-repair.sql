-- V18: repair the known V13/V14 list-cursor ledger divergence (khive #1649).
--
-- Some databases recorded V13 and V14 in `_schema_migrations` under names
-- other than the canonical `list_cursor_sequences` / `graph_edges_id_unique`
-- (a deployment-time renaming, not a schema difference). Because the
-- migration runner fast-forwards on version alone, those databases never
-- re-ran the V13/V14 DDL under their canonical identity, so this migration
-- re-asserts the V13/V14 effects unconditionally before normalizing the
-- recorded names -- safe on a database where those effects already exist,
-- since every statement below is idempotent (`IF NOT EXISTS` /
-- `INSERT OR IGNORE`), and the only path that actually repairs a divergent
-- database.

-- Recreate the three sequence tables if a divergent baseline never created
-- them under their canonical DDL.
CREATE TABLE IF NOT EXISTS entities_seq (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS notes_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_notes_seq_note_id ON notes_seq(note_id);

CREATE TABLE IF NOT EXISTS graph_edges_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id TEXT NOT NULL UNIQUE
);

-- Reasserted before the graph backfill below so a duplicate edge id across
-- namespaces fails this migration's own transaction loudly, matching the
-- V13 guard, instead of silently collapsing onto one shared ledger row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_id_unique ON graph_edges(id);

-- Backfill any entity, note, or graph edge missing its ledger row, in the
-- same canonical order V13 used: entities and notes by `(created_at, id)`,
-- graph edges by `id` alone (preserving the pre-V13 public edge cursor
-- order). `INSERT OR IGNORE` / the anti-join make each statement a no-op for
-- rows a prior, canonically-named V13/V14 already ledgered.
INSERT OR IGNORE INTO entities_seq (entity_id)
SELECT id FROM entities ORDER BY created_at ASC, id ASC;

INSERT OR IGNORE INTO notes_seq (note_id)
SELECT n.id FROM notes n
WHERE NOT EXISTS (SELECT 1 FROM notes_seq s WHERE s.note_id = n.id)
ORDER BY n.created_at ASC, n.id ASC;

INSERT OR IGNORE INTO graph_edges_seq (edge_id)
SELECT id FROM graph_edges ORDER BY id ASC;

-- Recreate the canonical assignment triggers so future inserts keep the
-- ledgers in sync, regardless of what a divergent baseline installed.
CREATE TRIGGER IF NOT EXISTS assign_entity_list_seq
AFTER INSERT ON entities
BEGIN
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

-- Recorded-name normalization for versions 13/14 happens in the migration
-- runner (migrations.rs), inside the same transaction that applies this
-- file: `_schema_migrations` is created and owned by the runner, so this
-- file stays applicable on a bare migration chain.
