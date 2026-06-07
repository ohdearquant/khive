-- Knowledge pack schema: atoms, domains, sections, and FTS indexes.
-- All tables are created together as a single logical unit.

CREATE TABLE IF NOT EXISTS knowledge_atoms (
    id          TEXT    PRIMARY KEY,
    namespace   TEXT    NOT NULL,
    slug        TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    description TEXT,
    tags        TEXT    NOT NULL DEFAULT '[]',
    properties  TEXT,
    finalized   INTEGER NOT NULL DEFAULT 0,
    status      TEXT    NOT NULL DEFAULT 'draft',
    source_uri  TEXT,
    source_type TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    deleted_at  INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_slug     ON knowledge_atoms(namespace, slug);
CREATE        INDEX IF NOT EXISTS idx_knowledge_atoms_ns           ON knowledge_atoms(namespace);
CREATE        INDEX IF NOT EXISTS idx_knowledge_atoms_ns_created   ON knowledge_atoms(namespace, created_at DESC);
CREATE        INDEX IF NOT EXISTS idx_knowledge_atoms_ns_status    ON knowledge_atoms(namespace, status);

CREATE TABLE IF NOT EXISTS knowledge_domains (
    id          TEXT    PRIMARY KEY,
    namespace   TEXT    NOT NULL,
    slug        TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    description TEXT,
    tags        TEXT    NOT NULL DEFAULT '[]',
    members     TEXT    NOT NULL DEFAULT '[]',
    status      TEXT    NOT NULL DEFAULT 'draft',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    deleted_at  INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_domains_ns_slug   ON knowledge_domains(namespace, slug);
CREATE        INDEX IF NOT EXISTS idx_knowledge_domains_ns         ON knowledge_domains(namespace);
CREATE        INDEX IF NOT EXISTS idx_knowledge_domains_ns_status  ON knowledge_domains(namespace, status);

CREATE TABLE IF NOT EXISTS knowledge_sections (
    id           TEXT    PRIMARY KEY,
    atom_id      TEXT    NOT NULL,
    namespace    TEXT    NOT NULL,
    section_type TEXT    NOT NULL,
    heading      TEXT    NOT NULL DEFAULT '',
    content      TEXT    NOT NULL DEFAULT '',
    content_hash TEXT    NOT NULL DEFAULT '',
    tokens       INTEGER NOT NULL DEFAULT 0,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    status       TEXT    NOT NULL DEFAULT 'draft',
    embedding    BLOB,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    FOREIGN KEY (atom_id) REFERENCES knowledge_atoms(id),
    UNIQUE(atom_id, content_hash)
);
CREATE        INDEX IF NOT EXISTS idx_knowledge_sections_atom      ON knowledge_sections(atom_id);
CREATE        INDEX IF NOT EXISTS idx_knowledge_sections_ns_type   ON knowledge_sections(namespace, section_type);
CREATE        INDEX IF NOT EXISTS idx_knowledge_sections_ns_atom   ON knowledge_sections(namespace, atom_id);
CREATE        INDEX IF NOT EXISTS idx_knowledge_sections_status    ON knowledge_sections(status);

-- FTS5 index over atoms (slug, name, description).
CREATE VIRTUAL TABLE IF NOT EXISTS fts_knowledge
    USING fts5(
        id          UNINDEXED,
        namespace   UNINDEXED,
        slug,
        name,
        description,
        content=knowledge_atoms,
        content_rowid=rowid,
        tokenize='trigram case_sensitive 0'
    );
CREATE TRIGGER IF NOT EXISTS fts_knowledge_ai
    AFTER INSERT ON knowledge_atoms
    WHEN new.deleted_at IS NULL BEGIN
    INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, description)
        VALUES(new.rowid, new.id, new.namespace, new.slug, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS fts_knowledge_ad
    AFTER DELETE ON knowledge_atoms BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, description)
        VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.description);
END;
CREATE TRIGGER IF NOT EXISTS fts_knowledge_au
    AFTER UPDATE ON knowledge_atoms BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, description)
        VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.description);
    INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, description)
        SELECT new.rowid, new.id, new.namespace, new.slug, new.name, new.description
        WHERE new.deleted_at IS NULL;
END;

-- FTS5 index over sections (heading, content).
CREATE VIRTUAL TABLE IF NOT EXISTS fts_sections
    USING fts5(
        id           UNINDEXED,
        namespace    UNINDEXED,
        atom_id      UNINDEXED,
        section_type UNINDEXED,
        heading,
        content,
        content=knowledge_sections,
        content_rowid=rowid,
        tokenize='trigram case_sensitive 0'
    );
CREATE TRIGGER IF NOT EXISTS fts_sections_ai
    AFTER INSERT ON knowledge_sections BEGIN
    INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content);
END;
CREATE TRIGGER IF NOT EXISTS fts_sections_ad
    AFTER DELETE ON knowledge_sections BEGIN
    INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content);
END;
CREATE TRIGGER IF NOT EXISTS fts_sections_au
    AFTER UPDATE ON knowledge_sections BEGIN
    INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content);
    INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content);
END;
