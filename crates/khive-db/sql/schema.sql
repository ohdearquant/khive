-- khive complete schema (v0.2.8) — single-shot DDL for fresh databases.
-- Generated from the cumulative migration state. No incremental migrations.

CREATE TABLE IF NOT EXISTS _embedding_models (id              BLOB PRIMARY KEY,engine_name     TEXT NOT NULL,model_id        TEXT NOT NULL,key_version     TEXT NOT NULL,dim             INTEGER NOT NULL,output_dim      INTEGER,status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'superseded', 'archived')),activated_at    INTEGER,superseded_at   INTEGER,superseded_by   BLOB,canonical_key   BLOB NOT NULL UNIQUE,created_at      INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS brain_event_log (id         INTEGER PRIMARY KEY AUTOINCREMENT,profile_id TEXT NOT NULL,namespace  TEXT NOT NULL DEFAULT 'default',event_kind TEXT NOT NULL,payload    TEXT NOT NULL,created_at INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS brain_profile_snapshots (profile_id    TEXT NOT NULL,namespace     TEXT NOT NULL DEFAULT 'default',snapshot_json TEXT NOT NULL,updated_at    INTEGER NOT NULL,PRIMARY KEY (profile_id, namespace));

CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY,namespace TEXT NOT NULL,kind TEXT NOT NULL,name TEXT NOT NULL,description TEXT,properties TEXT,tags TEXT NOT NULL DEFAULT '[]',created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,deleted_at INTEGER, entity_type TEXT NULL, merged_into TEXT, merge_event_id TEXT);

CREATE TABLE IF NOT EXISTS event_observations (event_id TEXT NOT NULL,entity_id TEXT NOT NULL,referent_kind TEXT NOT NULL,role TEXT NOT NULL,position INTEGER NOT NULL,PRIMARY KEY (event_id, role, position));

CREATE TABLE IF NOT EXISTS events (id TEXT PRIMARY KEY,namespace TEXT NOT NULL,verb TEXT NOT NULL,substrate TEXT NOT NULL,actor TEXT NOT NULL,outcome TEXT NOT NULL,data TEXT,duration_us INTEGER NOT NULL DEFAULT 0,target_id TEXT,created_at INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'audit', payload TEXT NOT NULL DEFAULT '{}', payload_schema_version INTEGER NOT NULL DEFAULT 1, profile_state_version INTEGER, session_id TEXT, aggregate_kind TEXT, aggregate_id TEXT);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_knowledge
    USING fts5(
        id          UNINDEXED,
        namespace   UNINDEXED,
        slug,
        name,
        content,
        content=knowledge_atoms,
        content_rowid=rowid,
        tokenize='trigram case_sensitive 0'
    );

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

CREATE TABLE IF NOT EXISTS "graph_edges" (namespace TEXT NOT NULL,id TEXT NOT NULL,source_id TEXT NOT NULL,target_id TEXT NOT NULL,relation TEXT NOT NULL,weight REAL NOT NULL DEFAULT 1.0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,deleted_at INTEGER,metadata TEXT,target_backend TEXT,PRIMARY KEY (namespace, id));

CREATE TABLE IF NOT EXISTS knowledge_atoms (
    id          TEXT    PRIMARY KEY,
    namespace   TEXT    NOT NULL,
    slug        TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    content     TEXT    NOT NULL DEFAULT '',
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

CREATE TABLE IF NOT EXISTS "notes" (id TEXT PRIMARY KEY,namespace TEXT NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'active',name TEXT,content TEXT NOT NULL DEFAULT '',salience REAL,decay_factor REAL,expires_at INTEGER,properties TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,deleted_at INTEGER);

CREATE TABLE IF NOT EXISTS proposals_open (proposal_id    TEXT PRIMARY KEY,namespace      TEXT NOT NULL,proposer       TEXT NOT NULL,title          TEXT NOT NULL,status         TEXT NOT NULL CHECK (status IN ('open', 'changes_requested', 'approved', 'applying', 'rejected', 'applied', 'withdrawn')),created_at     INTEGER NOT NULL,updated_at     INTEGER NOT NULL,expiry         INTEGER,last_decision  TEXT,review_count   INTEGER NOT NULL DEFAULT 0,approve_count  INTEGER NOT NULL DEFAULT 0,reject_count   INTEGER NOT NULL DEFAULT 0);

CREATE INDEX IF NOT EXISTS idx_brain_events_profile ON brain_event_log(profile_id, namespace, created_at);

CREATE INDEX IF NOT EXISTS idx_embed_models_engine_status ON _embedding_models(engine_name, status);

CREATE UNIQUE INDEX IF NOT EXISTS idx_embed_models_one_active ON _embedding_models(engine_name) WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);

CREATE INDEX IF NOT EXISTS idx_entities_kind_entity_type ON entities(namespace, kind, entity_type);

CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);

CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);

CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);

CREATE INDEX IF NOT EXISTS idx_event_obs_entity ON event_observations(entity_id, role);

CREATE INDEX IF NOT EXISTS idx_event_obs_event_role ON event_observations(event_id, role);

CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);

CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);

CREATE INDEX IF NOT EXISTS idx_events_ns_created ON events(namespace, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_events_ns_created_id ON events(namespace, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'));

CREATE INDEX IF NOT EXISTS idx_events_session ON events(namespace, session_id, created_at, id);

CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);

CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);

CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_relation ON graph_edges(namespace, relation);

CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_source ON graph_edges(namespace, source_id);

CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_src_rel ON graph_edges(namespace, source_id, relation);

CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_target ON graph_edges(namespace, target_id);

CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_tgt_rel ON graph_edges(namespace, target_id, relation);

CREATE INDEX IF NOT EXISTS idx_graph_edges_target_backend ON graph_edges(target_backend) WHERE target_backend IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_unique_triple ON graph_edges(namespace, source_id, target_id, relation);

CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns           ON knowledge_atoms(namespace);

CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_created   ON knowledge_atoms(namespace, created_at DESC);

CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_slug     ON knowledge_atoms(namespace, slug);

CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_status    ON knowledge_atoms(namespace, status);

CREATE INDEX IF NOT EXISTS idx_knowledge_domains_ns         ON knowledge_domains(namespace);

CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_domains_ns_slug   ON knowledge_domains(namespace, slug);

CREATE INDEX IF NOT EXISTS idx_knowledge_domains_ns_status  ON knowledge_domains(namespace, status);

CREATE INDEX IF NOT EXISTS idx_knowledge_sections_atom      ON knowledge_sections(atom_id);

CREATE INDEX IF NOT EXISTS idx_knowledge_sections_ns_atom   ON knowledge_sections(namespace, atom_id);

CREATE INDEX IF NOT EXISTS idx_knowledge_sections_ns_type   ON knowledge_sections(namespace, section_type);

CREATE INDEX IF NOT EXISTS idx_knowledge_sections_status    ON knowledge_sections(status);

CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);

CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);

CREATE INDEX IF NOT EXISTS idx_proposals_open_ns_status ON proposals_open(namespace, status);

CREATE INDEX IF NOT EXISTS idx_proposals_open_proposer ON proposals_open(namespace, proposer);

CREATE INDEX IF NOT EXISTS idx_proposals_open_updated ON proposals_open(namespace, updated_at DESC);

CREATE TRIGGER IF NOT EXISTS fts_knowledge_ad
    AFTER DELETE ON knowledge_atoms BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.content);

END;

CREATE TRIGGER IF NOT EXISTS fts_knowledge_ai
    AFTER INSERT ON knowledge_atoms
    WHEN new.deleted_at IS NULL BEGIN
    INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, content)
        VALUES(new.rowid, new.id, new.namespace, new.slug, new.name, new.content);

END;

CREATE TRIGGER IF NOT EXISTS fts_knowledge_au
    AFTER UPDATE ON knowledge_atoms BEGIN
    INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.content);

INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, content)
        SELECT new.rowid, new.id, new.namespace, new.slug, new.name, new.content
        WHERE new.deleted_at IS NULL;

END;

CREATE TRIGGER IF NOT EXISTS fts_sections_ad
    AFTER DELETE ON knowledge_sections BEGIN
    INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content);

END;

CREATE TRIGGER IF NOT EXISTS fts_sections_ai
    AFTER INSERT ON knowledge_sections BEGIN
    INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content);

END;

CREATE TRIGGER IF NOT EXISTS fts_sections_au
    AFTER UPDATE ON knowledge_sections BEGIN
    INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content);

INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content)
        VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content);

END;
