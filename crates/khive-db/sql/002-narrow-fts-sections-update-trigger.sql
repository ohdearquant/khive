-- V2: Narrow fts_sections update trigger to text-column changes only.
--
-- The unconditional AFTER UPDATE trigger fired on embedding-only UPDATEs
-- (section reindex), causing 358K pointless FTS5 delete+reinsert cycles
-- that bloated the WAL and corrupted the FTS shadow tables.

DROP TRIGGER IF EXISTS fts_sections_au;

CREATE TRIGGER fts_sections_au
AFTER UPDATE OF heading, content, section_type, namespace, atom_id ON knowledge_sections BEGIN
    INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES ('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content);
    INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES (new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content);
END;
