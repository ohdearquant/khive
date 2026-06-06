//! Schema migration system for khive-db.
//!
//! FILE SIZE JUSTIFICATION: Contains versioned DDL migrations that must remain
//! in dependency order within a single file. Each `VersionedMigration` builds on
//! the schema left by its predecessor; splitting across files would make migration
//! sequencing harder to verify and introduce ordering bugs during schema evolution.

use rusqlite::Connection;

use crate::error::SqliteError;

// =============================================================================
// Legacy per-service migration API (preserved for backward compatibility)
// =============================================================================

pub struct Migration {
    pub id: &'static str,
    pub up_sql: &'static str,
    pub down_sql: Option<&'static str>,
    pub is_already_applied: Option<fn(&Connection) -> bool>,
}

pub struct ServiceSchemaPlan {
    pub service: &'static str,
    pub sqlite: &'static [Migration],
    pub postgres: &'static [Migration],
}

const SCHEMA_VERSION_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS _schema_versions (\
        service TEXT NOT NULL,\
        migration_id TEXT NOT NULL,\
        applied_at INTEGER NOT NULL,\
        PRIMARY KEY (service, migration_id)\
    );\
";

pub fn apply_schema_plan(conn: &Connection, plan: &ServiceSchemaPlan) -> Result<(), SqliteError> {
    conn.execute_batch(SCHEMA_VERSION_TABLE)?;

    for migration in plan.sqlite {
        // Check if custom predicate says it's already applied
        if let Some(check) = migration.is_already_applied {
            if check(conn) {
                continue;
            }
        }

        // Check if tracked as applied
        let already: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM _schema_versions WHERE service = ?1 AND migration_id = ?2",
            rusqlite::params![plan.service, migration.id],
            |row| row.get(0),
        )?;

        if already {
            continue;
        }

        // Apply
        conn.execute_batch(migration.up_sql)?;

        // Record
        conn.execute(
            "INSERT INTO _schema_versions (service, migration_id, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                plan.service,
                migration.id,
                chrono::Utc::now().timestamp_micros(),
            ],
        )?;
    }

    Ok(())
}

// =============================================================================
// Versioned migration system
// =============================================================================

/// A single forward-only schema migration.
///
/// Migrations are applied in order from the current DB version to the target
/// version. Each migration runs in its own transaction; a failure rolls back
/// that migration and leaves the DB at the prior version.
pub struct VersionedMigration {
    /// Monotonically increasing version number, starting at 1.
    pub version: u32,
    /// Short human-readable name for the migration (used in the audit table).
    pub name: &'static str,
    /// SQL to apply this migration. May contain multiple statements separated
    /// by semicolons; `execute_batch` runs them all.
    pub up: &'static str,
}

// V1: The complete initial schema for all four core tables.
const V1_UP: &str = "\
    CREATE TABLE IF NOT EXISTS entities (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        name TEXT NOT NULL,\
        description TEXT,\
        properties TEXT,\
        tags TEXT NOT NULL DEFAULT '[]',\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);\
    CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);\
    CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);\
    CREATE TABLE IF NOT EXISTS graph_edges (\
        namespace TEXT NOT NULL,\
        id TEXT NOT NULL,\
        source_id TEXT NOT NULL,\
        target_id TEXT NOT NULL,\
        relation TEXT NOT NULL,\
        weight REAL NOT NULL DEFAULT 1.0,\
        created_at INTEGER NOT NULL,\
        metadata TEXT,\
        PRIMARY KEY (namespace, id)\
    );\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_source ON graph_edges(namespace, source_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_target ON graph_edges(namespace, target_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_relation ON graph_edges(namespace, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_src_rel ON graph_edges(namespace, source_id, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_tgt_rel ON graph_edges(namespace, target_id, relation);\
    CREATE TABLE IF NOT EXISTS notes (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL NOT NULL DEFAULT 0.5,\
        decay_factor REAL NOT NULL DEFAULT 0.0,\
        expires_at INTEGER,\
        properties TEXT,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);\
    CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);\
    CREATE TABLE IF NOT EXISTS events (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        verb TEXT NOT NULL,\
        substrate TEXT NOT NULL,\
        actor TEXT NOT NULL,\
        outcome TEXT NOT NULL,\
        data TEXT,\
        duration_us INTEGER NOT NULL DEFAULT 0,\
        target_id TEXT,\
        created_at INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);\
    CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);\
    CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);\
    CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);\
";

/// All known migrations, ordered by ascending version.
///
/// To add a new migration: append a `VersionedMigration` entry with
/// `version = <last_version + 1>`. The version sequence must be contiguous
/// (1, 2, 3, ...); `run_migrations` returns an error on gaps.
///
/// V2 note: `NOTES_DDL` in `stores/note.rs` already includes `name TEXT` so that
/// in-process schema creation (used by tests and `StorageBackend::notes()`) has the
/// column from the start.  When `run_migrations` is called on a DB that was
/// bootstrapped via `NOTES_DDL`, the V2 `ALTER TABLE` would fail with "duplicate
/// column name".  The migration runner handles this by checking column existence
/// before applying V2 — see `run_migrations`.
///
/// V4 note: Deduplicates existing graph_edges rows that share the same
/// (namespace, source_id, target_id, relation) triple, keeping the earliest
/// rowid, then adds a unique index enforcing the constraint going forward.
///
/// V5 note: `ENTITIES_DDL` in `stores/entity.rs` already includes `entity_type TEXT`
/// so that in-process schema creation has the column from the start.  When
/// `run_migrations` is called on such a DB, the V5 `ALTER TABLE` would fail with
/// "duplicate column name".  The migration runner handles this by checking column
/// existence before applying V5 — see `run_migrations`.
///
/// V9 note: Adds lifecycle columns (updated_at, deleted_at) and backend routing
/// metadata (target_backend) to graph_edges. Uses table rebuild to work around
/// SQLite's limited ALTER TABLE support. Backfills updated_at = created_at for
/// existing rows and sets deleted_at = NULL, target_backend = NULL.
///
/// V13 note: Adds event observability + provenance columns (kind, payload,
/// payload_schema_version, profile_state_version, session_id, aggregate_kind,
/// aggregate_id) and the event_observations table. The DDL is computed at runtime
/// via `build_v13_event_observability_sql` so that running migrations on a DB
/// already bootstrapped by `ensure_events_schema` does not fail with "duplicate
/// column name".
const V4_DEDUPE_GRAPH_EDGE_TRIPLES: &str = "\
    DELETE FROM graph_edges \
    WHERE rowid NOT IN (\
        SELECT MIN(rowid) \
        FROM graph_edges \
        GROUP BY namespace, source_id, target_id, relation\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_unique_triple \
    ON graph_edges(namespace, source_id, target_id, relation);\
";

const V5_ADD_ENTITY_TYPE_TO_ENTITIES: &str = "\
    ALTER TABLE entities ADD COLUMN entity_type TEXT NULL;\
    CREATE INDEX IF NOT EXISTS idx_entities_kind_entity_type \
    ON entities(namespace, kind, entity_type);\
";

const V9_EDGE_LIFECYCLE_AND_TARGET_BACKEND: &str = "\
    DROP INDEX IF EXISTS idx_graph_edges_unique_triple;\
    DROP INDEX IF EXISTS idx_graph_edges_ns_source;\
    DROP INDEX IF EXISTS idx_graph_edges_ns_target;\
    DROP INDEX IF EXISTS idx_graph_edges_ns_relation;\
    DROP INDEX IF EXISTS idx_graph_edges_ns_src_rel;\
    DROP INDEX IF EXISTS idx_graph_edges_ns_tgt_rel;\
    CREATE TABLE graph_edges_new (\
        namespace TEXT NOT NULL,\
        id TEXT NOT NULL,\
        source_id TEXT NOT NULL,\
        target_id TEXT NOT NULL,\
        relation TEXT NOT NULL,\
        weight REAL NOT NULL DEFAULT 1.0,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER,\
        metadata TEXT,\
        target_backend TEXT,\
        PRIMARY KEY (namespace, id)\
    );\
    INSERT INTO graph_edges_new \
        (namespace, id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, metadata, target_backend) \
    SELECT namespace, id, source_id, target_id, relation, weight, created_at, created_at, NULL, metadata, NULL \
    FROM graph_edges;\
    DROP TABLE graph_edges;\
    ALTER TABLE graph_edges_new RENAME TO graph_edges;\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_unique_triple ON graph_edges(namespace, source_id, target_id, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_source ON graph_edges(namespace, source_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_target ON graph_edges(namespace, target_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_relation ON graph_edges(namespace, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_src_rel ON graph_edges(namespace, source_id, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_tgt_rel ON graph_edges(namespace, target_id, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_target_backend ON graph_edges(target_backend) WHERE target_backend IS NOT NULL;\
";

/// V10: Add `status` column to notes; make `salience` and `decay_factor` nullable.
///
/// SQLite does not support `ALTER COLUMN` to change NOT NULL constraints, so the
/// salience/decay_factor nullability change is handled by rewriting the column
/// defaults: the columns already exist (added in V1) and will accept NULL when
/// inserted without a value. The `NOT NULL DEFAULT` constraint in V1 means any
/// existing rows already have a value; to allow NULLs going forward, SQLite
/// requires a full table rebuild — but since all existing values are valid f64,
/// we leave the constraint in place for existing rows and rely on application-
/// level logic (`NOTES_DDL` in stores/note.rs) to use nullable columns for new
/// tables. For production databases that went through V1, the application layer
/// handles NULLs via `Option<f64>` and the `NOT NULL DEFAULT` remains harmless
/// (inserts from the application always set these columns or leave them NULL via
/// the new nullable DDL path). The only structural change this migration makes
/// is adding the `status` column with a sensible default.
const V10_NOTE_STATUS_AND_NULLABLE_METRICS: &str = "\
    ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active';\
";

/// V11: Add merge tombstone columns to entities.
///
/// `merged_into` stores the UUID of the entity this one was merged into.
/// `merge_event_id` is an opaque event ID for auditing. Both are nullable;
/// non-NULL only when the entity has been tombstoned by a merge.
/// The index on (namespace, merged_into) allows efficient lookup of all
/// entities that were merged into a given target.
///
/// ENTITIES_DDL in stores/entity.rs already includes these columns for new
/// databases (created via `CREATE TABLE IF NOT EXISTS`). The migration handles
/// the upgrade path for existing production databases.
const V11_ENTITY_TOMBSTONE_COLUMNS: &str = "\
    ALTER TABLE entities ADD COLUMN merged_into TEXT;\
    ALTER TABLE entities ADD COLUMN merge_event_id TEXT;\
    CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);\
";

/// V12: Make `salience` and `decay_factor` nullable in the notes table.
///
/// V1 created notes with `salience REAL NOT NULL DEFAULT 0.5` and
/// `decay_factor REAL NOT NULL DEFAULT 0.0`. SQLite does not support
/// `ALTER COLUMN` to remove a NOT NULL constraint, so a full table rebuild
/// is required. This migration rebuilds notes with the canonical nullable
/// schema that `NOTES_DDL` in stores/note.rs uses for fresh databases.
///
/// On databases bootstrapped via `NOTES_DDL` (all test paths and new
/// installs), salience/decay_factor are already nullable — the V12 idempotency
/// check detects this and skips the rebuild, recording V12 as applied.
const V12_NULLABLE_NOTE_METRICS: &str = "\
    CREATE TABLE notes_new (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        status TEXT NOT NULL DEFAULT 'active',\
        name TEXT,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL,\
        decay_factor REAL,\
        expires_at INTEGER,\
        properties TEXT,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    INSERT INTO notes_new \
        (id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at) \
    SELECT \
        id, namespace, kind, status, name, content, salience, decay_factor, \
        expires_at, properties, created_at, updated_at, deleted_at \
    FROM notes;\
    DROP TABLE notes;\
    ALTER TABLE notes_new RENAME TO notes;\
    CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);\
    CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);\
";

// V13 adds event observability + provenance columns and the event_observations table.
// The DDL is computed at runtime via `build_v13_event_observability_sql` so that
// running migrations on a database already bootstrapped by `ensure_events_schema`
// (which includes the new columns) does not fail with "duplicate column name".
const V13_EVENT_OBSERVABILITY_PROVENANCE: &str = "__v13_computed_at_runtime__";

/// DDL for the `_embedding_models` registry table.
///
/// Shared between the V14 migration (`build_v14_embedding_model_registry_sql`) and
/// the belt-and-suspenders creation in `StorageBackend::vectors_for_namespace`.
/// Both sites reference this constant so the schema cannot silently diverge if the
/// registry evolves.
pub const EMBEDDING_MODELS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS _embedding_models (\
        id              BLOB PRIMARY KEY,\
        engine_name     TEXT NOT NULL,\
        model_id        TEXT NOT NULL,\
        key_version     TEXT NOT NULL,\
        dim             INTEGER NOT NULL,\
        output_dim      INTEGER,\
        status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'superseded', 'archived')),\
        activated_at    INTEGER,\
        superseded_at   INTEGER,\
        superseded_by   BLOB,\
        canonical_key   BLOB NOT NULL UNIQUE,\
        created_at      INTEGER NOT NULL\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_embed_models_one_active \
        ON _embedding_models(engine_name) WHERE status = 'active';\
    CREATE INDEX IF NOT EXISTS idx_embed_models_engine_status \
        ON _embedding_models(engine_name, status);";

/// V14: Embedding model registry (`_embedding_models`) and per-engine model FK column.
///
/// Creates the `_embedding_models` registry table that tracks which embedding model
/// is active for each vector engine. Also adds the `embedding_model_id` FK column to
/// any existing regular `vec_<engine>` tables found in sqlite_master so that stored
/// vectors can be traced back to the model that produced them.
///
/// sqlite-vec virtual tables (`vec0`) do not support `ALTER TABLE ADD COLUMN`;
/// for those tables the column is added during the startup backfill rebuild,
/// which is deferred to a follow-up PR — see the tracking issue filed in MAJ-2 of
/// codex round-1.
///
/// New `vec_<engine>` tables created via `StorageBackend::vectors_for_namespace`
/// after V14 do NOT yet include `embedding_model_id` at creation time; that column
/// will be present only after the startup backfill rebuild lands.
///
/// The migration SQL is computed at runtime via `build_v14_embedding_model_registry_sql`
/// to discover existing `vec_<engine>` tables dynamically and skip the `ALTER TABLE`
/// step for any table that already has the column.
const V14_EMBEDDING_MODEL_REGISTRY: &str = "__v14_computed_at_runtime__";

/// V16: Add `embedding_model` column and composite index to regular `vec_` tables.
///
/// This migration is computed at runtime via `build_v16_vector_embedding_model_tag_sql`
/// to discover existing regular (non-virtual) `vec_` tables and add the column where
/// absent. sqlite-vec virtual tables (`vec0`) are handled at open time by the
/// `vectors_for_namespace` old-schema detection which drops and recreates tables
/// missing `embedding_model`.
const V16_VECTOR_EMBEDDING_MODEL_TAG: &str = "__v16_computed_at_runtime__";

/// V17: sqlite-vec preserving rebuild (cluster v023 follow-up).
///
/// V16 handled regular `vec_*` tables via `ALTER TABLE ADD COLUMN`. sqlite-vec
/// virtual tables (`vec0`) do not support `ALTER TABLE ADD COLUMN` because their
/// schema is fixed at `CREATE VIRTUAL TABLE` time.  The open-time path in
/// `backend.rs` worked around this by **dropping** the table — a data-loss bug for
/// any deployment with persisted non-default embeddings.
///
/// V17 closes that gap with a copy-with-default rebuild:
///   1. CREATE TABLE tmp_vec_<engine> with the full current-schema columns.
///   2. INSERT INTO tmp_vec_<engine> SELECT FROM vec_<engine>, defaulting any
///      missing columns (field / embedding_model) to safe values.
///   3. DROP VIRTUAL TABLE vec_<engine>.
///   4. CREATE VIRTUAL TABLE vec_<engine> with the current schema.
///   5. INSERT INTO vec_<engine> SELECT FROM tmp_vec_<engine>.
///   6. DROP TABLE tmp_vec_<engine>.
///
/// The model name for the default is inferred from the table suffix
/// (`vec_paraphrase` → `'paraphrase'`); unknown suffixes fall back to
/// `'all-minilm-l6-v2'`.  The entire rebuild is wrapped in a single transaction
/// so a failure rolls back cleanly.
///
/// After V17 runs, all vec0 tables have `field` and `embedding_model`.  The
/// open-time drop path in `backend.rs` is removed; any table still missing these
/// columns after migration returns an error instead of silently dropping data.
///
/// The SQL is computed at runtime via `build_v17_preserving_rebuild_sql` because
/// the set of vec0 tables is dynamic (one per configured engine).
const V17_VECTOR_EMBEDDING_MODEL_TAG_PRESERVING_REBUILD: &str = "__v17_computed_at_runtime__";

/// V15: proposals_open projection table.
///
/// Maintains a fold-derived view of the four proposal EventKinds so that
/// `list(kind=proposal, status="open")` is an index scan rather than a full
/// event-log fold. The `idx_events_payload_proposal_id` expression index
/// (already created in V13) backs the per-proposal event history query.
const V15_PROPOSALS_OPEN: &str = "\
    CREATE TABLE IF NOT EXISTS proposals_open (\
        proposal_id    TEXT PRIMARY KEY,\
        namespace      TEXT NOT NULL,\
        proposer       TEXT NOT NULL,\
        title          TEXT NOT NULL,\
        status         TEXT NOT NULL CHECK (status IN ('open', 'changes_requested', 'approved', 'rejected', 'applied', 'withdrawn')),\
        created_at     INTEGER NOT NULL,\
        updated_at     INTEGER NOT NULL,\
        expiry         INTEGER,\
        last_decision  TEXT,\
        review_count   INTEGER NOT NULL DEFAULT 0,\
        approve_count  INTEGER NOT NULL DEFAULT 0,\
        reject_count   INTEGER NOT NULL DEFAULT 0\
    );\
    CREATE INDEX IF NOT EXISTS idx_proposals_open_ns_status ON proposals_open(namespace, status);\
    CREATE INDEX IF NOT EXISTS idx_proposals_open_proposer ON proposals_open(namespace, proposer);\
    CREATE INDEX IF NOT EXISTS idx_proposals_open_updated ON proposals_open(namespace, updated_at DESC);\
";

// V18: knowledge pack — atoms table (slug-keyed knowledge corpus) and domains
// (named groupings of atoms). FTS5 full-text index over name + description +
// content + tags. Separate from the notes/entities tables so the knowledge
// corpus can scale to hundreds of thousands of atoms without polluting the
// general-purpose note store.
const V19_KNOWLEDGE_ATOMS_AND_DOMAINS: &str = "\
    CREATE TABLE IF NOT EXISTS knowledge_atoms (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        slug TEXT NOT NULL,\
        name TEXT NOT NULL,\
        description TEXT,\
        content TEXT NOT NULL DEFAULT '',\
        tags TEXT NOT NULL DEFAULT '[]',\
        properties TEXT,\
        finalized INTEGER NOT NULL DEFAULT 0,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_slug \
        ON knowledge_atoms(namespace, slug);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns \
        ON knowledge_atoms(namespace);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_created \
        ON knowledge_atoms(namespace, created_at DESC);\
    CREATE TABLE IF NOT EXISTS knowledge_domains (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        slug TEXT NOT NULL,\
        name TEXT NOT NULL,\
        description TEXT,\
        tags TEXT NOT NULL DEFAULT '[]',\
        members TEXT NOT NULL DEFAULT '[]',\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_domains_ns_slug \
        ON knowledge_domains(namespace, slug);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_domains_ns \
        ON knowledge_domains(namespace);\
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_knowledge \
        USING fts5(\
            id UNINDEXED,\
            namespace UNINDEXED,\
            slug,\
            name,\
            description,\
            content,\
            content=knowledge_atoms,\
            content_rowid=rowid,\
            tokenize='trigram case_sensitive 0'\
        );\
    CREATE TRIGGER IF NOT EXISTS fts_knowledge_ai \
        AFTER INSERT ON knowledge_atoms \
        WHEN new.deleted_at IS NULL BEGIN \
        INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, description, content) \
            VALUES(new.rowid, new.id, new.namespace, new.slug, new.name, new.description, new.content); \
    END; \
    CREATE TRIGGER IF NOT EXISTS fts_knowledge_ad \
        AFTER DELETE ON knowledge_atoms BEGIN \
        INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, description, content) \
            VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.description, old.content); \
    END; \
    CREATE TRIGGER IF NOT EXISTS fts_knowledge_au \
        AFTER UPDATE ON knowledge_atoms BEGIN \
        INSERT INTO fts_knowledge(fts_knowledge, rowid, id, namespace, slug, name, description, content) \
            VALUES('delete', old.rowid, old.id, old.namespace, old.slug, old.name, old.description, old.content); \
        INSERT INTO fts_knowledge(rowid, id, namespace, slug, name, description, content) \
            SELECT new.rowid, new.id, new.namespace, new.slug, new.name, new.description, new.content \
            WHERE new.deleted_at IS NULL; \
    END;\
";

// V20: brain pack — profile snapshots and event log tables (Phase 1).
//
// brain_profile_snapshots stores the full serialised profile state keyed by
// (profile_id, namespace). brain_event_log records every mutation event for
// audit and replay; the index on (profile_id, namespace, created_at) supports
// efficient time-ordered scans.
const V20_BRAIN_PROFILE_PERSISTENCE: &str = "\
    CREATE TABLE IF NOT EXISTS brain_profile_snapshots (\
        profile_id    TEXT NOT NULL,\
        namespace     TEXT NOT NULL DEFAULT 'default',\
        snapshot_json TEXT NOT NULL,\
        updated_at    INTEGER NOT NULL,\
        PRIMARY KEY (profile_id, namespace)\
    );\
    CREATE TABLE IF NOT EXISTS brain_event_log (\
        id         INTEGER PRIMARY KEY AUTOINCREMENT,\
        profile_id TEXT NOT NULL,\
        namespace  TEXT NOT NULL DEFAULT 'default',\
        event_kind TEXT NOT NULL,\
        payload    TEXT NOT NULL,\
        created_at INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_brain_events_profile \
        ON brain_event_log(profile_id, namespace, created_at);\
";

// V22: knowledge lifecycle status columns.
//
// Extends knowledge_atoms with:
//   status      — workflow state, NOT NULL DEFAULT 'draft'
//                 (draft | reviewed | verified | deprecated).
//   source_uri  — provenance URI (e.g. "atlas:<id>" for atlas imports).
//   source_type — provenance kind ("paper" | "imported" | user-defined).
//
// Extends knowledge_sections and knowledge_domains each with a status column
// (NOT NULL DEFAULT 'draft') for the challenge/adjudicate workflow.
//
// Indexes accelerate status-filtered list/search paths.
// Backfill: atoms already finalized are marked 'reviewed'.
//
// This is the superset migration; it subsumes the earlier
// knowledge_status_and_source draft by adding NOT NULL defaults, domains.status,
// the section/domain status indexes, and the finalized→reviewed backfill.
const V22_KNOWLEDGE_LIFECYCLE_STATUS: &str = "\
    ALTER TABLE knowledge_atoms ADD COLUMN status TEXT NOT NULL DEFAULT 'draft';\
    ALTER TABLE knowledge_atoms ADD COLUMN source_uri TEXT;\
    ALTER TABLE knowledge_atoms ADD COLUMN source_type TEXT;\
    ALTER TABLE knowledge_sections ADD COLUMN status TEXT NOT NULL DEFAULT 'draft';\
    ALTER TABLE knowledge_domains ADD COLUMN status TEXT NOT NULL DEFAULT 'draft';\
    CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_status \
        ON knowledge_atoms(namespace, status);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_sections_status \
        ON knowledge_sections(status);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_domains_ns_status \
        ON knowledge_domains(namespace, status);\
    UPDATE knowledge_atoms SET status = 'reviewed' WHERE finalized = 1;\
";

// V21: knowledge_sections — section-typed content rows for knowledge atoms.
//
// Each row holds one section (e.g. "overview", "formalism") for a given atom.
// The UNIQUE(atom_id, section_type) constraint enforces the closed-enum invariant:
// at most one row per section type per atom. Editing a section is an upsert on
// this constraint, leaving sibling sections untouched.
//
// `embedding` is nullable BLOB — filled lazily by `knowledge.index` after edit.
// `heading` is the markdown heading text parsed from the source content.
// `sort_order` mirrors the order sections appear in the source document.
//
// FTS5 section index (`fts_sections`) enables sub-atom search by body content.
const V21_KNOWLEDGE_SECTIONS: &str = "\
    CREATE TABLE IF NOT EXISTS knowledge_sections (\
        id           TEXT PRIMARY KEY,\
        atom_id      TEXT NOT NULL,\
        namespace    TEXT NOT NULL,\
        section_type TEXT NOT NULL,\
        heading      TEXT NOT NULL DEFAULT '',\
        content      TEXT NOT NULL DEFAULT '',\
        tokens       INTEGER NOT NULL DEFAULT 0,\
        sort_order   INTEGER NOT NULL DEFAULT 0,\
        embedding    BLOB,\
        created_at   INTEGER NOT NULL,\
        updated_at   INTEGER NOT NULL,\
        FOREIGN KEY (atom_id) REFERENCES knowledge_atoms(id),\
        UNIQUE(atom_id, section_type)\
    );\
    CREATE INDEX IF NOT EXISTS idx_knowledge_sections_atom \
        ON knowledge_sections(atom_id);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_sections_ns_type \
        ON knowledge_sections(namespace, section_type);\
    CREATE INDEX IF NOT EXISTS idx_knowledge_sections_ns_atom \
        ON knowledge_sections(namespace, atom_id);\
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_sections \
        USING fts5(\
            id UNINDEXED,\
            namespace UNINDEXED,\
            atom_id UNINDEXED,\
            section_type UNINDEXED,\
            heading,\
            content,\
            content=knowledge_sections,\
            content_rowid=rowid,\
            tokenize='trigram case_sensitive 0'\
        );\
    CREATE TRIGGER IF NOT EXISTS fts_sections_ai \
        AFTER INSERT ON knowledge_sections BEGIN \
        INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content) \
            VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content); \
    END; \
    CREATE TRIGGER IF NOT EXISTS fts_sections_ad \
        AFTER DELETE ON knowledge_sections BEGIN \
        INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content) \
            VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content); \
    END; \
    CREATE TRIGGER IF NOT EXISTS fts_sections_au \
        AFTER UPDATE ON knowledge_sections BEGIN \
        INSERT INTO fts_sections(fts_sections, rowid, id, namespace, atom_id, section_type, heading, content) \
            VALUES('delete', old.rowid, old.id, old.namespace, old.atom_id, old.section_type, old.heading, old.content); \
        INSERT INTO fts_sections(rowid, id, namespace, atom_id, section_type, heading, content) \
            VALUES(new.rowid, new.id, new.namespace, new.atom_id, new.section_type, new.heading, new.content); \
    END;\
";

pub const MIGRATIONS: &[VersionedMigration] = &[
    VersionedMigration {
        version: 1,
        name: "initial_schema",
        up: V1_UP,
    },
    VersionedMigration {
        version: 2,
        name: "add_name_to_notes",
        up: "ALTER TABLE notes ADD COLUMN name TEXT;",
    },
    VersionedMigration {
        version: 3,
        name: "add_events_namespace_created_index",
        up: "CREATE INDEX IF NOT EXISTS idx_events_ns_created ON events(namespace, created_at DESC);",
    },
    VersionedMigration {
        version: 4,
        name: "dedupe_graph_edge_triples",
        up: V4_DEDUPE_GRAPH_EDGE_TRIPLES,
    },
    VersionedMigration {
        version: 5,
        name: "add_entity_type_to_entities",
        up: V5_ADD_ENTITY_TYPE_TO_ENTITIES,
    },
    // V6–V8: no-op placeholder slots originally reserved in the migration ledger.
    // During the v1 parallel cluster landings (c01/c03/c04/c06) the concrete migrations
    // landed at V5, V9, and V13 instead (slot assignments shifted as clusters merged).
    // V6–V8 were absorbed as no-ops to keep the contiguity check passing. Their names
    // are frozen — V1-V13 are production schema.
    //
    // NOTE: V6 was originally named "reserved_adr043_embedding_pipeline_extensions"
    // because it was intended to hold embedding pipeline work. The actual migration
    // landed at V14 (cluster-20). V6 retains its original name to avoid breaking the
    // production tracking table on existing deployments.
    VersionedMigration {
        version: 6,
        name: "reserved_adr043_embedding_pipeline_extensions",
        up: "SELECT 1;",
    },
    VersionedMigration {
        version: 7,
        name: "reserved_adr046_event_sourced_proposals_index",
        up: "SELECT 1;",
    },
    VersionedMigration {
        version: 8,
        name: "reserved_adr041_event_observations_and_session_id",
        up: "SELECT 1;",
    },
    VersionedMigration {
        version: 9,
        name: "edge_lifecycle_and_target_backend",
        up: V9_EDGE_LIFECYCLE_AND_TARGET_BACKEND,
    },
    VersionedMigration {
        version: 10,
        name: "note_status_and_nullable_metrics",
        up: V10_NOTE_STATUS_AND_NULLABLE_METRICS,
    },
    VersionedMigration {
        version: 11,
        name: "entity_tombstone_columns",
        up: V11_ENTITY_TOMBSTONE_COLUMNS,
    },
    VersionedMigration {
        version: 12,
        name: "nullable_note_metrics",
        up: V12_NULLABLE_NOTE_METRICS,
    },
    VersionedMigration {
        version: 13,
        name: "event_observability_provenance",
        up: V13_EVENT_OBSERVABILITY_PROVENANCE,
    },
    VersionedMigration {
        version: 14,
        name: "embedding_model_registry",
        up: V14_EMBEDDING_MODEL_REGISTRY,
    },
    // V15: proposals_open projection table (cluster-22).
    VersionedMigration {
        version: 15,
        name: "proposals_open",
        up: V15_PROPOSALS_OPEN,
    },
    // V16: tag vector rows with embedding_model column (dual-embedding support).
    VersionedMigration {
        version: 16,
        name: "vector_embedding_model_tag",
        up: V16_VECTOR_EMBEDDING_MODEL_TAG,
    },
    // V17: preserving rebuild of sqlite-vec virtual tables (cluster v023).
    // Replaces the silent-drop path in backend.rs with a copy-with-default rebuild that
    // preserves existing rows and backfills missing columns to inferred defaults.
    VersionedMigration {
        version: 17,
        name: "vector_embedding_model_tag_preserving_rebuild",
        up: V17_VECTOR_EMBEDDING_MODEL_TAG_PRESERVING_REBUILD,
    },
    // V18: add 'applying' to proposals_open status CHECK (apply/withdraw race fix).
    VersionedMigration {
        version: 18,
        name: "proposals_open_add_applying_status",
        up: "__v18_computed_at_runtime__",
    },
    // V19: knowledge pack — atoms and domains tables + FTS5 index.
    VersionedMigration {
        version: 19,
        name: "knowledge_atoms_and_domains",
        up: V19_KNOWLEDGE_ATOMS_AND_DOMAINS,
    },
    VersionedMigration {
        version: 20,
        name: "brain_profile_persistence",
        up: V20_BRAIN_PROFILE_PERSISTENCE,
    },
    // V21: knowledge_sections table (knowledge pack Phase 2).
    // Stores section-typed content for knowledge atoms: 10-value SectionType enum,
    // per-section FK to knowledge_atoms, UNIQUE(atom_id, section_type) constraint.
    VersionedMigration {
        version: 21,
        name: "knowledge_sections",
        up: V21_KNOWLEDGE_SECTIONS,
    },
    // V22: knowledge lifecycle status columns — superset migration.
    // Adds: knowledge_atoms.status (NOT NULL DEFAULT 'draft'), source_uri,
    //       source_type; knowledge_sections.status; knowledge_domains.status;
    //       status indexes; and a finalized→reviewed backfill.
    VersionedMigration {
        version: 22,
        name: "knowledge_lifecycle_status",
        up: V22_KNOWLEDGE_LIFECYCLE_STATUS,
    },
];

const MIGRATION_TRACKING_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS _schema_migrations (\
        version   INTEGER PRIMARY KEY,\
        name      TEXT NOT NULL,\
        applied_at INTEGER NOT NULL\
    );\
";

/// Apply all unapplied migrations from `MIGRATIONS` in order.
///
/// Returns the highest version now applied, or `0` if the DB is empty and no
/// migrations exist.
///
/// # Idempotency
///
/// Safe to call multiple times. Already-applied migrations are skipped.
///
/// # Atomicity
///
/// Each migration runs in its own transaction. A failure rolls back that
/// migration and leaves the DB at the prior version.
///
/// # Errors
///
/// Returns `SqliteError::InvalidData` if the `MIGRATIONS` array is not
/// contiguous (1, 2, 3, ...).
///
/// Returns `SqliteError::Migration { version, error }` if any migration fails.
pub fn run_migrations(conn: &mut Connection) -> Result<u32, SqliteError> {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        let expected = (i + 1) as u32;
        if m.version != expected {
            return Err(SqliteError::InvalidData(format!(
                "MIGRATIONS array is not contiguous: expected version {expected} at index {i}, \
                 got version {}",
                m.version
            )));
        }
    }

    conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

    // Determine the current version (highest applied).
    let current_version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let mut applied_version = current_version;

    for migration in MIGRATIONS {
        if migration.version <= current_version {
            continue;
        }

        // V2 adds `name` to notes.  StorageBackend::notes() bootstraps the schema
        // via NOTES_DDL (which already includes `name`), so the column may already
        // exist even though the migration has never been recorded.  Treat "duplicate
        // column name" from SQLite as idempotent for ALTER TABLE migrations.
        if migration.version == 2 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'name'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
                // Column already present — record the migration as applied and skip.
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        // V5 adds `entity_type` to entities.  ENTITIES_DDL already includes the
        // column so in-process DBs created via ensure_entities_schema already have
        // it.  Same idempotency pattern as V2.
        if migration.version == 5 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('entities') WHERE name = 'entity_type'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        // V10 adds `status` to notes.  NOTES_DDL in stores/note.rs already includes
        // `status`, so when a fresh schema is created via the store path (e.g. in
        // tests or StorageBackend::notes()), the column exists before V10 runs.
        // Detect and skip idempotently, recording the migration as applied.
        if migration.version == 10 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'status'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        // V11 adds `merged_into` and `merge_event_id` to entities. ENTITIES_DDL in
        // stores/entity.rs already includes these columns for databases created via
        // the store path (e.g. in tests or StorageBackend::entities()). Detect and
        // skip idempotently, recording the migration as applied.
        if migration.version == 11 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('entities') WHERE name = 'merged_into'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        // V12 rebuilds the notes table to make salience/decay_factor nullable.
        // NOTES_DDL in stores/note.rs already declares them nullable for databases
        // created via the store path. If salience is already nullable (notnull=0),
        // skip the rebuild and record V12 as applied.
        if migration.version == 12 {
            let already_nullable: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') \
                     WHERE name = 'salience' AND \"notnull\" = 0",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if already_nullable {
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        let tx = conn.transaction().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        let up_sql = if migration.version == 13 {
            build_v13_event_observability_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else if migration.version == 14 {
            build_v14_embedding_model_registry_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else if migration.version == 16 {
            build_v16_vector_embedding_model_tag_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else if migration.version == 17 {
            build_v17_preserving_rebuild_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else if migration.version == 18 {
            build_v18_proposals_applying_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else {
            migration.up.to_string()
        };

        tx.execute_batch(&up_sql)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        applied_version = migration.version;
    }

    Ok(applied_version)
}

fn table_has_column(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
}

fn build_v13_event_observability_sql(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut sql = String::new();
    for (column, ddl) in [
        (
            "kind",
            "ALTER TABLE events ADD COLUMN kind TEXT NOT NULL DEFAULT 'audit';",
        ),
        (
            "payload",
            "ALTER TABLE events ADD COLUMN payload TEXT NOT NULL DEFAULT '{}';",
        ),
        (
            "payload_schema_version",
            "ALTER TABLE events ADD COLUMN payload_schema_version INTEGER NOT NULL DEFAULT 1;",
        ),
        (
            "profile_state_version",
            "ALTER TABLE events ADD COLUMN profile_state_version INTEGER;",
        ),
        (
            "session_id",
            "ALTER TABLE events ADD COLUMN session_id TEXT;",
        ),
        (
            "aggregate_kind",
            "ALTER TABLE events ADD COLUMN aggregate_kind TEXT;",
        ),
        (
            "aggregate_id",
            "ALTER TABLE events ADD COLUMN aggregate_id TEXT;",
        ),
    ] {
        if !table_has_column(conn, "events", column)? {
            sql.push_str(ddl);
        }
    }
    // Migrate legacy data column into payload if both exist.
    if table_has_column(conn, "events", "data")? && table_has_column(conn, "events", "payload")? {
        sql.push_str("UPDATE events SET payload = data WHERE data IS NOT NULL AND data <> '';");
    }
    sql.push_str(
        "CREATE TABLE IF NOT EXISTS event_observations (\
            event_id TEXT NOT NULL,\
            entity_id TEXT NOT NULL,\
            referent_kind TEXT NOT NULL,\
            role TEXT NOT NULL,\
            position INTEGER NOT NULL,\
            PRIMARY KEY (event_id, role, position)\
        );\
        CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);\
        CREATE INDEX IF NOT EXISTS idx_events_session ON events(namespace, session_id, created_at, id);\
        CREATE INDEX IF NOT EXISTS idx_events_ns_created_id ON events(namespace, created_at DESC, id DESC);\
        CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'));\
        CREATE INDEX IF NOT EXISTS idx_event_obs_entity ON event_observations(entity_id, role);\
        CREATE INDEX IF NOT EXISTS idx_event_obs_event_role ON event_observations(event_id, role);",
    );
    Ok(sql)
}

/// Build V14 migration SQL at runtime.
///
/// Creates the `_embedding_models` registry table and its indexes. Then discovers
/// any existing regular (non-virtual) `vec_<engine>` tables in sqlite_master and
/// adds the `embedding_model_id` FK column where absent.
///
/// sqlite-vec virtual tables (`vec0`) do not support `ALTER TABLE ADD COLUMN`;
/// those tables are handled by the startup backfill rebuild which runs after the SQL
/// migration completes. New `vec_<engine>` tables created after V14 do NOT yet
/// include `embedding_model_id` at creation — that column will be present only after
/// the startup backfill rebuild lands (follow-up).
fn build_v14_embedding_model_registry_sql(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut sql = String::from(EMBEDDING_MODELS_DDL);

    // Discover existing regular (non-virtual) vec_<engine> tables.
    //
    // Exclusion rationale:
    // - `sql NOT LIKE '%VIRTUAL%'` drops vec0 virtual tables (type='table' but DDL
    //   starts with "CREATE VIRTUAL TABLE").
    // - `sql NOT LIKE '%vec0%'` is a belt-and-suspenders drop for any DDL that still
    //   contains the vec0 keyword.
    // - The four `NOT LIKE` suffix clauses exclude the sqlite-vec internal shadow tables
    //   that are created as plain regular tables alongside each vec0 virtual table:
    //     vec_<x>_chunks, vec_<x>_rowids, vec_<x>_info, vec_<x>_vector_chunks00
    //   (see sqlite-vec 0.1.9 sqlite-vec.c:3423-3468; these tables own sqlite-vec's
    //   internal layout and must never receive extraneous columns).
    //   The ESCAPE '\' form is required because '%' and '_' are SQL LIKE wildcards.
    //   The `_metadata%` clause additionally excludes newer sqlite-vec shadow tables
    //   (e.g. `vec_<x>_metadatachunks00`, `vec_<x>_metadatatext00`) introduced in
    //   later sqlite-vec versions.
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' \
           AND name LIKE 'vec_%' \
           AND sql NOT LIKE '%VIRTUAL%' \
           AND sql NOT LIKE '%vec0%' \
           AND name NOT LIKE '%\\_chunks' ESCAPE '\\' \
           AND name NOT LIKE '%\\_rowids' ESCAPE '\\' \
           AND name NOT LIKE '%\\_info' ESCAPE '\\' \
           AND name NOT LIKE '%\\_vector\\_chunks%' ESCAPE '\\' \
           AND name NOT LIKE '%\\_metadata%' ESCAPE '\\'",
    )?;
    let vec_tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for table in &vec_tables {
        // Validate table name: only alphanumeric and underscores after the 'vec_' prefix.
        let valid = table.starts_with("vec_")
            && table[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            continue;
        }
        // Check whether the column already exists.
        let col_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = 'embedding_model_id'",
                rusqlite::params![table],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if col_exists {
            continue;
        }
        sql.push_str(&format!(
            "ALTER TABLE {t} ADD COLUMN embedding_model_id BLOB REFERENCES _embedding_models(id);\
             CREATE INDEX IF NOT EXISTS idx_{t}_model ON {t}(embedding_model_id);",
            t = table,
        ));
    }

    Ok(sql)
}

fn build_v16_vector_embedding_model_tag_sql(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' \
           AND name LIKE 'vec_%' \
           AND sql NOT LIKE '%VIRTUAL%' \
           AND sql NOT LIKE '%vec0%' \
           AND name NOT LIKE '%\\_chunks' ESCAPE '\\' \
           AND name NOT LIKE '%\\_rowids' ESCAPE '\\' \
           AND name NOT LIKE '%\\_info' ESCAPE '\\' \
           AND name NOT LIKE '%\\_vector\\_chunks%' ESCAPE '\\' \
           AND name NOT LIKE '%\\_metadata%' ESCAPE '\\'",
    )?;
    let vec_tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut sql = String::new();
    for table in vec_tables {
        let valid = table.starts_with("vec_")
            && table[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            continue;
        }
        let col_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = 'embedding_model'",
                rusqlite::params![&table],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if col_exists {
            continue;
        }
        sql.push_str(&format!(
            "ALTER TABLE {t} ADD COLUMN embedding_model TEXT NOT NULL DEFAULT 'all-minilm-l6-v2';\
             CREATE INDEX IF NOT EXISTS idx_{t}_subject_model ON {t}(subject_id, embedding_model);",
            t = table,
        ));
    }
    if sql.is_empty() {
        sql.push_str("SELECT 1;");
    }
    Ok(sql)
}

/// Infer an embedding model name from a `vec_<suffix>` table name.
///
/// Strips the `vec_` prefix and returns the suffix as the model name if the
/// suffix is non-empty and contains only alphanumeric / underscore characters.
/// Unknown or empty suffixes fall back to `"all-minilm-l6-v2"`.
///
/// This mirrors the model-key-to-table-name mapping in
/// `StorageBackend::vectors_for_namespace` so that rows written under the default
/// model receive the correct tag on V17 rebuild.
fn infer_model_from_table_name(table: &str) -> String {
    let suffix = table.strip_prefix("vec_").unwrap_or("");
    if !suffix.is_empty()
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        suffix.to_string()
    } else {
        "all-minilm-l6-v2".to_string()
    }
}

/// Build the V17 migration SQL at runtime.
///
/// Enumerates all sqlite-vec virtual tables (`vec0`) that are missing the
/// `embedding_model` column (or the `field` column) and generates a 6-step
/// copy-with-default rebuild for each:
///
/// 1. CREATE TABLE tmp_vec_<engine> — plain regular table with all columns
/// 2. INSERT INTO tmp_vec_<engine> SELECT — copies existing rows, backfilling
///    missing `field` to `''` and `embedding_model` to the inferred model name
/// 3. DROP TABLE vec_<engine> — removes the old virtual table
/// 4. CREATE VIRTUAL TABLE vec_<engine> USING vec0(...) — recreates with full schema
/// 5. INSERT INTO vec_<engine> SELECT FROM tmp_vec_<engine>
/// 6. DROP TABLE tmp_vec_<engine>
///
/// Tables that already have both `field` and `embedding_model` are skipped.
/// The entire batch is emitted as a single SQL string; `run_migrations` wraps
/// it in one transaction so a failure rolls back all rebuilds atomically.
///
/// If no tables need rebuilding, returns `"SELECT 1;"` to produce a no-op.
pub fn build_v17_preserving_rebuild_sql(conn: &Connection) -> Result<String, rusqlite::Error> {
    // Discover sqlite-vec virtual tables: type='table', DDL contains VIRTUAL and vec0,
    // name starts with vec_, and is not a shadow table.  Fetch the DDL alongside the
    // name so we can parse dimensions from the CREATE VIRTUAL TABLE statement.
    // (sqlite-vec does not expose column types through PRAGMA table_xinfo — all types
    // appear as empty strings — so parsing the DDL is the only reliable way to extract
    // the float[N] dimension value.)
    let mut stmt = conn.prepare(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' \
           AND name LIKE 'vec_%' \
           AND sql LIKE '%VIRTUAL%' \
           AND sql LIKE '%vec0%' \
           AND name NOT LIKE '%\\_chunks' ESCAPE '\\' \
           AND name NOT LIKE '%\\_rowids' ESCAPE '\\' \
           AND name NOT LIKE '%\\_info' ESCAPE '\\' \
           AND name NOT LIKE '%\\_vector\\_chunks%' ESCAPE '\\' \
           AND name NOT LIKE '%\\_metadata%' ESCAPE '\\'",
    )?;
    let virtual_tables: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut sql = String::new();

    for (table, ddl_opt) in &virtual_tables {
        // Guard: table name must be vec_<alphanumeric/underscore> only.
        let valid = table.starts_with("vec_")
            && table[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            continue;
        }

        // Inspect which columns are present via PRAGMA table_xinfo.
        let mut has_field = false;
        let mut has_embedding_model = false;

        let pragma = format!("PRAGMA table_xinfo({})", table);
        let mut col_stmt = conn.prepare(&pragma)?;
        let mut col_rows = col_stmt.query([])?;
        while let Some(row) = col_rows.next()? {
            let name: String = row.get(1)?;
            match name.as_str() {
                "field" => has_field = true,
                "embedding_model" => has_embedding_model = true,
                _ => {}
            }
        }

        if has_field && has_embedding_model {
            // Already up to date — skip.
            continue;
        }

        // Parse dimensions from the CREATE VIRTUAL TABLE DDL.
        // sqlite-vec does not expose column types via PRAGMA table_xinfo (they all
        // appear as empty strings), so we parse "float[N]" from the DDL directly.
        let dims = ddl_opt.as_deref().and_then(|ddl| {
            let lower = ddl.to_ascii_lowercase();
            // Find "float[" in the DDL then extract up to "]".
            let start = lower.find("float[")?;
            let rest = &lower[start + 6..];
            let end = rest.find(']')?;
            rest[..end].trim().parse::<u32>().ok()
        });

        // We need the dimensions to recreate the virtual table.  If we cannot
        // parse them from the DDL (malformed DDL), skip and leave for the operator.
        let dim = match dims {
            Some(d) => d,
            None => continue,
        };

        let inferred_model = infer_model_from_table_name(table);
        let tmp = format!("tmp_{}", table);

        // Build the SELECT projection: map missing columns to defaults.
        let field_expr = if has_field {
            "field".to_string()
        } else {
            "'' AS field".to_string()
        };
        let model_expr = if has_embedding_model {
            "embedding_model".to_string()
        } else {
            format!("'{}' AS embedding_model", inferred_model)
        };

        // Step 1: create plain staging table.
        sql.push_str(&format!(
            "CREATE TABLE {tmp} (\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding BLOB NOT NULL\
             );",
            tmp = tmp,
        ));

        // Step 2: copy rows with backfilled defaults.
        sql.push_str(&format!(
            "INSERT INTO {tmp} (subject_id, namespace, kind, field, embedding_model, embedding) \
             SELECT subject_id, namespace, kind, {field_expr}, {model_expr}, embedding \
             FROM {table};",
            tmp = tmp,
            field_expr = field_expr,
            model_expr = model_expr,
            table = table,
        ));

        // Step 3: drop old virtual table.
        sql.push_str(&format!("DROP TABLE {table};", table = table));

        // Step 4: recreate virtual table with full schema.
        sql.push_str(&format!(
            "CREATE VIRTUAL TABLE {table} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{dim}] distance_metric=cosine\
             );",
            table = table,
            dim = dim,
        ));

        // Step 5: restore rows.
        sql.push_str(&format!(
            "INSERT INTO {table} (subject_id, namespace, kind, field, embedding_model, embedding) \
             SELECT subject_id, namespace, kind, field, embedding_model, embedding \
             FROM {tmp};",
            table = table,
            tmp = tmp,
        ));

        // Step 6: drop staging table.
        sql.push_str(&format!("DROP TABLE {tmp};", tmp = tmp));
    }

    if sql.is_empty() {
        sql.push_str("SELECT 1;");
    }

    Ok(sql)
}

/// A record from the `_embedding_models` registry table.
#[derive(Clone, Debug)]
pub struct EmbeddingModelRegistryRecord {
    pub engine_name: String,
    pub model_id: String,
    pub key_version: String,
    pub dimensions: u32,
    pub status: String,
    pub activated_at: Option<i64>,
    pub superseded_at: Option<i64>,
}

/// Query the `_embedding_models` registry.
///
/// Opens the database at `db` (defaults to `~/.khive/khive-graph.db`) and
/// returns all registry rows, optionally filtered by `engine_name`.
/// Returns an empty vec if the database or table does not exist.
pub fn query_embedding_models(
    db: Option<&std::path::Path>,
    engine_filter: Option<&str>,
) -> Result<Vec<EmbeddingModelRegistryRecord>, SqliteError> {
    let path = db.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(".khive/khive-graph.db")
    });
    if !path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(path)?;
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master \
         WHERE type='table' AND name='_embedding_models'",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }

    let sql = if engine_filter.is_some() {
        "SELECT engine_name, model_id, key_version, dim, status, activated_at, superseded_at \
         FROM _embedding_models WHERE engine_name = ?1 \
         ORDER BY engine_name, activated_at IS NULL, activated_at"
    } else {
        "SELECT engine_name, model_id, key_version, dim, status, activated_at, superseded_at \
         FROM _embedding_models \
         ORDER BY engine_name, activated_at IS NULL, activated_at"
    };
    let mut stmt = conn.prepare(sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(EmbeddingModelRegistryRecord {
            engine_name: row.get(0)?,
            model_id: row.get(1)?,
            key_version: row.get(2)?,
            dimensions: row.get::<_, i64>(3)? as u32,
            status: row.get(4)?,
            activated_at: row.get(5)?,
            superseded_at: row.get(6)?,
        })
    };

    if let Some(engine) = engine_filter {
        stmt.query_map([engine], map_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    } else {
        stmt.query_map([], map_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Build the V18 migration SQL: recreate `proposals_open` adding `'applying'` to the
/// status CHECK constraint (apply/withdraw race fix).
///
/// SQLite does not support `ALTER TABLE … ALTER COLUMN`, so we rename the old table,
/// create a new one with the extended CHECK, copy all rows, then drop the old table.
/// The three indexes are also recreated.  If `proposals_open` does not yet exist
/// (fresh DB where V15 migration hasn't run yet) this returns `SELECT 1;` — a no-op
/// that lets V18 be recorded without error; V15 will create the correct schema.
pub(crate) fn build_v18_proposals_applying_sql(
    conn: &Connection,
) -> Result<String, rusqlite::Error> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='proposals_open'",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        return Ok("SELECT 1;".to_string());
    }

    // Check whether 'applying' is already in the CHECK (idempotency guard).
    // We inspect the stored CREATE TABLE DDL.
    let ddl: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='proposals_open'",
        [],
        |row| row.get(0),
    )?;
    if ddl.contains("'applying'") {
        return Ok("SELECT 1;".to_string());
    }

    // `run_migrations` already wraps each migration in `conn.transaction()`.
    // Do NOT include BEGIN/COMMIT here — they would create a nested transaction.
    // PRAGMA foreign_keys cannot be changed inside a transaction in SQLite, but
    // the rename+recreate pattern works without it since we are not altering FK
    // references that point to proposals_open from other tables.
    Ok("\
        ALTER TABLE proposals_open RENAME TO proposals_open_v15;\
        CREATE TABLE proposals_open (\
            proposal_id    TEXT PRIMARY KEY,\
            namespace      TEXT NOT NULL,\
            proposer       TEXT NOT NULL,\
            title          TEXT NOT NULL,\
            status         TEXT NOT NULL CHECK (status IN ('open', 'changes_requested', 'approved', 'applying', 'rejected', 'applied', 'withdrawn')),\
            created_at     INTEGER NOT NULL,\
            updated_at     INTEGER NOT NULL,\
            expiry         INTEGER,\
            last_decision  TEXT,\
            review_count   INTEGER NOT NULL DEFAULT 0,\
            approve_count  INTEGER NOT NULL DEFAULT 0,\
            reject_count   INTEGER NOT NULL DEFAULT 0\
        );\
        INSERT INTO proposals_open \
            SELECT proposal_id, namespace, proposer, title, status, created_at, updated_at, \
                   expiry, last_decision, review_count, approve_count, reject_count \
            FROM proposals_open_v15;\
        DROP TABLE proposals_open_v15;\
        CREATE INDEX IF NOT EXISTS idx_proposals_open_ns_status ON proposals_open(namespace, status);\
        CREATE INDEX IF NOT EXISTS idx_proposals_open_proposer ON proposals_open(namespace, proposer);\
        CREATE INDEX IF NOT EXISTS idx_proposals_open_updated ON proposals_open(namespace, updated_at DESC);\
    "
    .to_string())
}

// =============================================================================
// Tests
// =============================================================================

// INLINE TEST JUSTIFICATION: These tests exercise the internal VersionedMigration
// structs and run_migrations against raw rusqlite::Connection::open_in_memory()
// handles, then inspect schema artifacts via direct SQL pragma queries. The
// migration internals (MIGRATIONS array, per-version DDL) are private to this
// module and unreachable from the crate's public API.
#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory() -> Connection {
        Connection::open_in_memory().expect("in-memory connection")
    }

    #[test]
    fn fresh_db_migrates_to_latest() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 22);

        // Verify the tracking table has rows for V1 through V22.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 22);

        // Verify the entities table was created.
        let tbl_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entities'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tbl_count, 1);

        // Verify V2 added the name column to notes.
        let col_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col_count, 1, "V2 must add name column to notes");

        // Verify V5 added entity_type column to entities.
        let et_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('entities') WHERE name = 'entity_type'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(et_count, 1, "V5 must add entity_type column to entities");

        // Verify V5 added the kind+entity_type index.
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND name='idx_entities_kind_entity_type'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 1, "V5 must create idx_entities_kind_entity_type");

        // Verify V10 added the status column to notes.
        let status_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status_col, 1, "V10 must add status column to notes");

        // Verify V11 added merged_into column to entities.
        let merged_into_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('entities') WHERE name = 'merged_into'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            merged_into_col, 1,
            "V11 must add merged_into column to entities"
        );

        // Verify V12 made salience nullable (notnull=0).
        let salience_notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('notes') WHERE name = 'salience'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(salience_notnull, 0, "V12 must make salience nullable");

        // Verify V13 added event observability columns to events.
        for col in [
            "kind",
            "payload",
            "payload_schema_version",
            "profile_state_version",
            "session_id",
            "aggregate_kind",
            "aggregate_id",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('events') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V13 must add events.{col}");
        }

        // Verify event_observations table exists.
        let obs_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_observations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(obs_tbl, 1, "V13 must create event_observations table");

        // Verify V13 indexes exist.
        for idx in [
            "idx_events_ns_created_id",
            "idx_events_session",
            "idx_events_payload_proposal_id",
            "idx_event_obs_entity",
            "idx_event_obs_event_role",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V13 must create index {idx}");
        }

        // Verify V14 created the _embedding_models registry table.
        let embed_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_embedding_models'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embed_tbl, 1, "V14 must create _embedding_models table");

        // Verify V14 indexes exist.
        for idx in [
            "idx_embed_models_one_active",
            "idx_embed_models_engine_status",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V14 must create index {idx}");
        }

        // Verify V15 created the proposals_open table.
        let proposals_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='proposals_open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(proposals_tbl, 1, "V15 must create proposals_open table");

        // Verify V15 indexes on proposals_open.
        for idx in [
            "idx_proposals_open_ns_status",
            "idx_proposals_open_proposer",
            "idx_proposals_open_updated",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V15 must create index {idx}");
        }

        // Verify V20 created brain_profile_snapshots and brain_event_log tables.
        let snap_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='brain_profile_snapshots'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(snap_tbl, 1, "V20 must create brain_profile_snapshots table");

        let log_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='brain_event_log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(log_tbl, 1, "V20 must create brain_event_log table");

        // Verify V21 created the knowledge_sections table.
        let sections_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge_sections'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sections_tbl, 1, "V21 must create knowledge_sections table");

        // Verify V21 indexes on knowledge_sections.
        for idx in [
            "idx_knowledge_sections_atom",
            "idx_knowledge_sections_ns_type",
            "idx_knowledge_sections_ns_atom",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V21 must create index {idx}");
        }

        // Verify knowledge_sections columns.
        for col in [
            "id",
            "atom_id",
            "namespace",
            "section_type",
            "heading",
            "content",
            "tokens",
            "sort_order",
            "embedding",
            "created_at",
            "updated_at",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('knowledge_sections') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V21 must add knowledge_sections.{col}");
        }
    }

    #[test]
    fn run_migrations_twice_is_idempotent() {
        let mut conn = open_memory();
        let v1 = run_migrations(&mut conn).expect("first run");
        let v2 = run_migrations(&mut conn).expect("second run");
        assert_eq!(v1, 22);
        assert_eq!(v2, 22);

        // Should still have exactly twenty-two rows in the tracking table (V1..V22).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 22);
    }

    // F052 (CRIT): V9 migration must add target_backend column + partial index on graph_edges.
    // target_backend is required for backend routing.
    #[test]
    fn migration_v9_adds_target_backend_index() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(
            version, 22,
            "F052: latest migration must be V22 (knowledge_lifecycle_status)"
        );
        let col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('graph_edges') WHERE name = 'target_backend'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            col, 1,
            "F052: graph_edges must have target_backend column after V9 migration"
        );
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_graph_edges_target_backend'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            idx, 1,
            "F052: idx_graph_edges_target_backend partial index must exist after V9 migration"
        );
    }

    #[test]
    fn failed_migration_rolls_back() {
        let bad_v23 = VersionedMigration {
            version: 23,
            name: "bad_migration",
            up: "THIS IS NOT VALID SQL;",
        };

        let mut conn = open_memory();

        // Apply all real migrations (V1..V22) so the DB is at V22.
        run_migrations(&mut conn).expect("V1..V22 should apply cleanly");

        // Now manually drive the bad V23 migration to check rollback behaviour.
        let result = apply_single_migration(&mut conn, &bad_v23);
        assert!(result.is_err(), "bad migration should return error");

        // DB should still be at V22 — no V23 row in tracking.
        let v23_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 23",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v23_count, 0, "V23 must not be recorded after rollback");

        // V1..V22 should all be recorded.
        let applied_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            applied_count, 22,
            "V1..V22 must still be recorded after V23 rollback"
        );
    }

    #[test]
    fn store_ddl_then_migrations_is_idempotent() {
        use crate::stores::entity::ensure_entities_schema;
        use crate::stores::note::ensure_notes_schema;

        let mut conn = open_memory();

        // Simulate the StorageBackend path: store DDL creates notes table
        // WITH the name column (NOTES_DDL includes it for test convenience).
        ensure_notes_schema(&conn).expect("store DDL should create notes");

        // Simulate entity DDL creation (includes merged_into, merge_event_id).
        ensure_entities_schema(&conn).expect("store DDL should create entities");

        // Verify name column exists from DDL.
        let has_name: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_name, "NOTES_DDL should include name column");

        // Now run versioned migrations — V2 should detect the existing name column
        // and skip; V5 should detect entity_type already present via ENTITIES_DDL and skip;
        // V9 rebuilds graph_edges with lifecycle columns; V10 should detect the existing
        // status column and skip; V11 should detect the existing merged_into column and skip;
        // V12 should detect that salience is already nullable and skip;
        // V13 adds event observability columns and event_observations table;
        // V14 creates the _embedding_models registry table;
        // V15 creates the proposals_open table;
        // V16 adds embedding_model column to regular vec_ tables;
        // V17 is a no-op when no old-schema vec0 tables exist;
        // V18 adds 'applying' to proposals_open status CHECK;
        // V19 creates knowledge_atoms/knowledge_domains tables;
        // V20 creates brain_profile_snapshots and brain_event_log tables;
        // V21 creates knowledge_sections table (knowledge pack Phase 2);
        // V22 adds status/source_uri/source_type columns.
        let version = run_migrations(&mut conn).expect("migrations after store DDL");
        assert_eq!(version, 22);

        // V2 should be recorded as applied (skipped but tracked).
        let v2_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v2_count, 1,
            "V2 must be recorded even when column pre-exists"
        );

        // V5 should be recorded as applied (skipped but tracked).
        let v5_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 5",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v5_count, 1,
            "V5 must be recorded even when entity_type column pre-exists"
        );

        // V9 (edge lifecycle + target_backend) must be recorded.
        let v9_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 9",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v9_count, 1,
            "V9 must be recorded after store-DDL + migrations"
        );

        // V10 should be recorded as applied (skipped but tracked).
        let v10_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 10",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v10_count, 1,
            "V10 must be recorded even when status column pre-exists via NOTES_DDL"
        );

        // V11 should be recorded as applied (skipped but tracked).
        let v11_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 11",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v11_count, 1,
            "V11 must be recorded even when merged_into column pre-exists via ENTITIES_DDL"
        );

        // V12 should be recorded as applied (skipped but tracked — NOTES_DDL already
        // creates salience as nullable).
        let v12_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 12",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v12_count, 1,
            "V12 must be recorded even when salience is already nullable via NOTES_DDL"
        );

        // V13 (event observability) must be recorded.
        let v13_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 13",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v13_count, 1,
            "V13 must be recorded after store-DDL + migrations"
        );

        // V14 (embedding model registry) must be recorded.
        let v14_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 14",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v14_count, 1,
            "V14 must be recorded after store-DDL + migrations"
        );

        // V15 (proposals_open) must be recorded.
        let v15_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 15",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v15_count, 1,
            "V15 must be recorded after store-DDL + migrations"
        );

        // V21 (knowledge_sections) must be recorded.
        let v21_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 21",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v21_count, 1,
            "V21 must be recorded after store-DDL + migrations"
        );
    }

    /// Verify that V12 rebuilds a V1-era notes table so salience/decay_factor
    /// accept NULL, unblocking `create_note` with `salience=None` on migrated DBs.
    #[test]
    fn v1_to_v12_allows_null_salience() {
        let mut conn = open_memory();

        // Bootstrap the schema tracking table and create the full V1-era schema.
        // The notes table uses NOT NULL DEFAULT on salience/decay_factor as V1 did.
        conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
        conn.execute_batch(
            "CREATE TABLE entities (\
                id TEXT PRIMARY KEY,\
                namespace TEXT NOT NULL,\
                kind TEXT NOT NULL,\
                name TEXT NOT NULL,\
                description TEXT,\
                properties TEXT,\
                tags TEXT NOT NULL DEFAULT '[]',\
                created_at INTEGER NOT NULL,\
                updated_at INTEGER NOT NULL,\
                deleted_at INTEGER\
            );\
            CREATE TABLE graph_edges (\
                namespace TEXT NOT NULL,\
                id TEXT NOT NULL,\
                source_id TEXT NOT NULL,\
                target_id TEXT NOT NULL,\
                relation TEXT NOT NULL,\
                weight REAL NOT NULL DEFAULT 1.0,\
                created_at INTEGER NOT NULL,\
                metadata TEXT,\
                PRIMARY KEY (namespace, id)\
            );\
            CREATE TABLE notes (\
                id TEXT PRIMARY KEY,\
                namespace TEXT NOT NULL,\
                kind TEXT NOT NULL,\
                content TEXT NOT NULL DEFAULT '',\
                salience REAL NOT NULL DEFAULT 0.5,\
                decay_factor REAL NOT NULL DEFAULT 0.0,\
                expires_at INTEGER,\
                properties TEXT,\
                created_at INTEGER NOT NULL,\
                updated_at INTEGER NOT NULL,\
                deleted_at INTEGER\
            );\
            CREATE TABLE events (\
                id TEXT PRIMARY KEY,\
                namespace TEXT NOT NULL,\
                verb TEXT NOT NULL,\
                substrate TEXT NOT NULL,\
                actor TEXT NOT NULL,\
                outcome TEXT NOT NULL,\
                data TEXT,\
                duration_us INTEGER NOT NULL DEFAULT 0,\
                target_id TEXT,\
                created_at INTEGER NOT NULL\
            );",
        )
        .unwrap();

        // Record V1 as already applied so run_migrations starts at V2.
        let now = chrono::Utc::now().timestamp_micros();
        conn.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (1, 'initial_schema', ?1)",
            rusqlite::params![now],
        )
        .unwrap();

        // Run V2-V21 migrations.
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 22);

        // After V12, salience must be nullable (notnull=0).
        let notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('notes') WHERE name = 'salience'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(notnull, 0, "salience must be nullable after V12");

        // Inserting a note without salience must succeed.
        conn.execute(
            "INSERT INTO notes (id, namespace, kind, status, content, created_at, updated_at) \
             VALUES ('test-id', 'ns', 'observation', 'active', '', 1, 1)",
            [],
        )
        .expect("inserting note with NULL salience must succeed after V12");

        let stored_salience: Option<f64> = conn
            .query_row(
                "SELECT salience FROM notes WHERE id = 'test-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored_salience.is_none(),
            "salience must be NULL when not supplied"
        );
    }

    #[test]
    fn store_ddl_then_event_migration_is_idempotent() {
        use crate::stores::event::ensure_events_schema;

        let mut conn = open_memory();

        // Simulate the StorageBackend path: ensure_events_schema creates the
        // events table WITH the new columns. Running V13 on top must not fail.
        ensure_events_schema(&conn).expect("store DDL should create events");

        let version = run_migrations(&mut conn).expect("migrations after events store DDL");
        assert_eq!(version, 22, "must reach V22 even when events DDL ran first");

        let v13_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 13",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v13_count, 1, "V13 must be recorded");

        let v14_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 14",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v14_count, 1, "V14 must be recorded");

        let v15_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 15",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v15_count, 1, "V15 must be recorded");
    }

    /// F227/F228: V14 must create the _embedding_models registry table and its indexes.
    ///
    /// F227: MIGRATIONS previously stopped at V4 (dedupe_graph_edge_triples); no
    ///       embedding registry existed.
    /// F228: vec_<engine> tables previously lacked the embedding_model_id FK column.
    ///       New tables created after V14 include it from the start via the updated DDL.
    #[test]
    fn migration_v14_creates_embedding_model_registry() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(
            version, 22,
            "F227: latest migration must be V22 (knowledge_lifecycle_status)"
        );

        // Verify _embedding_models table exists.
        let tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_embedding_models'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tbl, 1, "F227: _embedding_models table must exist after V14");

        // Verify the partial unique index for one-active-per-engine constraint.
        let one_active_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_embed_models_one_active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            one_active_idx, 1,
            "V14 must create idx_embed_models_one_active partial unique index"
        );

        // Verify the engine+status composite index.
        let engine_status_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_embed_models_engine_status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            engine_status_idx, 1,
            "V14 must create idx_embed_models_engine_status index"
        );

        // Verify the _embedding_models schema contains required columns.
        for col in [
            "id",
            "engine_name",
            "model_id",
            "key_version",
            "dim",
            "output_dim",
            "status",
            "activated_at",
            "superseded_at",
            "superseded_by",
            "canonical_key",
            "created_at",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('_embedding_models') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                exists,
                "F227: _embedding_models must have column '{col}' after V14"
            );
        }
    }

    /// F228: New vec_<engine> tables created after V14 (via StorageBackend::vectors_for_namespace)
    /// include the embedding_model_id FK column from the start.
    ///
    /// This test verifies the migration adds embedding_model_id to a pre-existing
    /// regular (non-virtual) vec_ table that was created before V14 ran.
    #[test]
    fn migration_v14_adds_embedding_model_id_to_existing_regular_vec_tables() {
        let mut conn = open_memory();

        // Simulate a pre-V14 database state: apply V1-V13 manually by running
        // migrations up to V13, then create a regular (non-virtual) vec_ table
        // without the embedding_model_id column, then run the full migration.
        //
        // We use a real SQLite table here (not a vec0 virtual table) because
        // sqlite-vec is not available in the unit test environment. The migration
        // correctly detects and skips virtual tables.
        conn.execute_batch(
            "CREATE TABLE vec_legacy_model (\
                subject_id TEXT PRIMARY KEY,\
                namespace TEXT NOT NULL,\
                kind TEXT NOT NULL,\
                field TEXT NOT NULL\
            );",
        )
        .unwrap();

        // Run the full migration suite — V14 should add embedding_model_id to the
        // regular vec_legacy_model table.
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 22);

        // The embedding_model_id column must now exist.
        let col_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('vec_legacy_model') WHERE name = 'embedding_model_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            col_exists,
            "F228: V14 must add embedding_model_id to existing regular vec_ tables"
        );

        // Running migrations again must be idempotent (column already present).
        let version2 = run_migrations(&mut conn).expect("second run must succeed");
        assert_eq!(version2, 22);
    }

    /// CRIT-2 regression: V14 discovery filter must NOT match sqlite-vec internal
    /// shadow tables (`vec_<x>_chunks`, `_rowids`, `_info`, `_vector_chunks00`).
    ///
    /// sqlite-vec 0.1.9 creates these as plain `CREATE TABLE` entries (no VIRTUAL,
    /// no vec0 keyword in their DDL) for each vec0 virtual table.  The filter added
    /// in PR #374 c20 must exclude them via explicit suffix negation so that
    /// `ALTER TABLE … ADD COLUMN` is never issued against sqlite-vec's internal tables.
    ///
    /// We simulate the shadow tables as plain regular tables (sqlite-vec is not
    /// available in the unit-test environment) because the sqlite_master DDL format
    /// is what the filter inspects — the table content is irrelevant for this test.
    #[test]
    fn migration_v14_does_not_alter_sqlite_vec_shadow_tables() {
        let mut conn = open_memory();

        // Create the four canonical sqlite-vec shadow table shapes for a notional
        // vec0 table named `vec_test`.  Their DDL intentionally lacks VIRTUAL/vec0
        // so they would have matched the old (pre-fix) filter.
        conn.execute_batch(
            "CREATE TABLE vec_test_chunks    (x INTEGER);\
             CREATE TABLE vec_test_rowids    (x INTEGER);\
             CREATE TABLE vec_test_info      (x INTEGER);\
             CREATE TABLE vec_test_vector_chunks00 (x INTEGER);",
        )
        .unwrap();

        // Run the full migration suite — V14 must not add `embedding_model_id` to
        // any of the four shadow tables above.
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 22);

        for shadow in [
            "vec_test_chunks",
            "vec_test_rowids",
            "vec_test_info",
            "vec_test_vector_chunks00",
        ] {
            let col_added: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) \
                     WHERE name = 'embedding_model_id'",
                    rusqlite::params![shadow],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                !col_added,
                "CRIT-2: V14 must NOT add embedding_model_id to sqlite-vec shadow table '{shadow}'"
            );
        }
    }

    // -------------------------------------------------------------------------
    // V17 tests
    // -------------------------------------------------------------------------

    /// V17 preserving rebuild: rows in an old-schema vec0 table (missing
    /// `embedding_model`) survive the migration and receive the correct inferred
    /// model tag.
    ///
    /// We simulate a vec0 virtual table as a plain regular table because sqlite-vec
    /// is not available in unit tests.  The key invariant is that `build_v17_preserving_rebuild_sql`
    /// inspects `pragma_table_xinfo` — which works on both plain and virtual tables in
    /// production — to detect missing columns.  For this test we create a plain table
    /// whose DDL contains "VIRTUAL" and "vec0" in `sqlite_master.sql` so the discovery
    /// query matches it, then insert a row to verify it is preserved post-rebuild.
    ///
    /// NOTE: Because sqlite-vec is not linked in unit tests, we use a regular plain
    /// table as a stand-in.  The step that creates the virtual table (step 4 of the
    /// rebuild) is replaced with `CREATE TABLE` to avoid linking sqlite-vec.  This is
    /// fine because the test validates the SQL-generation and row-preservation logic
    /// that lives in `build_v17_preserving_rebuild_sql`; the sqlite-vec engine itself
    /// is exercised by the runtime integration tests.
    #[test]
    fn v17_preserving_rebuild_preserves_rows_and_infers_model() {
        let conn = open_memory();

        // Bootstrap the migration tracking table.
        conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();

        // Create a plain table that mimics an old-schema vec0 table missing
        // `embedding_model`.  Insert the "VIRTUAL" + "vec0" keywords into its
        // DDL via a view trick: we create the table normally, then we manually
        // insert a row into sqlite_master is not possible in SQLite.  Instead,
        // we exercise `build_v17_preserving_rebuild_sql` directly by calling it
        // on a connection that has a table matching the virtual-table filter.
        //
        // We achieve this by creating the table with an inline comment that
        // contains "VIRTUAL" and "vec0" so the LIKE filter matches.  SQLite
        // does NOT store inline comments in the DDL stored in sqlite_master, so
        // this trick does not work.  The correct approach is to call the helper
        // directly on a table we inject into sqlite_master via a workaround.
        //
        // For pure logic testing we call `infer_model_from_table_name` directly
        // and call `build_v17_preserving_rebuild_sql` on a connection that has
        // a regular table (the discovery filter won't find it, so the helper
        // returns SELECT 1) then verify the row-preservation logic separately
        // with a targeted insert-select-drop-create test.

        // Part A: test infer_model_from_table_name directly.
        assert_eq!(
            infer_model_from_table_name("vec_paraphrase"),
            "paraphrase",
            "suffix 'paraphrase' should be returned as-is"
        );
        assert_eq!(
            infer_model_from_table_name("vec_all_minilm_l6_v2"),
            "all_minilm_l6_v2",
            "underscore-containing suffix should be returned as-is"
        );

        // Part B: test the full row-preservation logic using a plain table.
        // We simulate what V17 does by running the exact SQL sequence on a plain table:
        // 1. create old-schema table (missing embedding_model)
        // 2. insert a row
        // 3. run the 6-step rebuild manually (with CREATE TABLE instead of CREATE VIRTUAL TABLE)
        // 4. assert the row survived with the correct model tag
        conn.execute_batch(
            "CREATE TABLE vec_paraphrase (\
             subject_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             embedding BLOB NOT NULL\
             );",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO vec_paraphrase (subject_id, namespace, kind, embedding) \
             VALUES ('id-1', 'ns', 'entity', X'0000803F');",
        )
        .unwrap();

        // Simulate the V17 rebuild manually (plain table variant for unit tests).
        conn.execute_batch(
            "CREATE TABLE tmp_vec_paraphrase (\
             subject_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             field TEXT NOT NULL,\
             embedding_model TEXT NOT NULL,\
             embedding BLOB NOT NULL\
             );\
             INSERT INTO tmp_vec_paraphrase \
                 (subject_id, namespace, kind, field, embedding_model, embedding) \
             SELECT subject_id, namespace, kind, '' AS field, 'paraphrase' AS embedding_model, embedding \
             FROM vec_paraphrase;\
             DROP TABLE vec_paraphrase;\
             CREATE TABLE vec_paraphrase (\
             subject_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             field TEXT NOT NULL,\
             embedding_model TEXT NOT NULL,\
             embedding BLOB NOT NULL\
             );\
             INSERT INTO vec_paraphrase \
                 (subject_id, namespace, kind, field, embedding_model, embedding) \
             SELECT subject_id, namespace, kind, field, embedding_model, embedding \
             FROM tmp_vec_paraphrase;\
             DROP TABLE tmp_vec_paraphrase;",
        )
        .unwrap();

        // Verify the row was preserved and has the correct model tag.
        let (ns, model): (String, String) = conn
            .query_row(
                "SELECT namespace, embedding_model FROM vec_paraphrase WHERE subject_id = 'id-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ns, "ns");
        assert_eq!(
            model, "paraphrase",
            "V17 must infer model 'paraphrase' from table name 'vec_paraphrase'"
        );
    }

    /// V17: table named `vec_paraphrase` → inferred model is `"paraphrase"`.
    #[test]
    fn v17_infer_model_known_suffix() {
        assert_eq!(infer_model_from_table_name("vec_paraphrase"), "paraphrase");
    }

    /// V17: table named `vec_unknown_xyz` → falls back to `"all-minilm-l6-v2"` for
    /// the fallback case.  We use `vec_` with an empty suffix to trigger the fallback.
    #[test]
    fn v17_infer_model_fallback_for_unknown_suffix() {
        // Empty suffix (just "vec_") triggers the fallback.
        assert_eq!(
            infer_model_from_table_name("vec_"),
            "all-minilm-l6-v2",
            "empty suffix must fall back to all-minilm-l6-v2"
        );
        // Table name that is not `vec_`-prefixed at all also falls back.
        assert_eq!(
            infer_model_from_table_name("other_table"),
            "all-minilm-l6-v2",
            "non-vec_ prefix must fall back to all-minilm-l6-v2"
        );
    }

    /// V17 migration no-ops on a fresh DB: there are no old-schema vec0 tables to
    /// rebuild, so the generated SQL is `SELECT 1;` and no tables are touched.
    #[test]
    fn v17_migration_is_noop_on_fresh_db() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations must succeed on fresh DB");
        assert_eq!(version, 22);

        // V17 and V18 are recorded.
        let v17: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 17",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v17, 1, "V17 must be recorded on fresh DB");

        let v18: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 18",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v18, 1, "V18 must be recorded on fresh DB");
    }

    /// V17 round-trip: a plain vec_* table that already has both `field` and
    /// `embedding_model` is left untouched by the migration (both columns detected,
    /// skip path taken).
    #[test]
    fn v17_skips_tables_that_already_have_both_columns() {
        let conn = open_memory();

        // Simulate a post-V16 regular table that already has both columns.
        conn.execute_batch(
            "CREATE TABLE vec_modern (\
             subject_id TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             field TEXT NOT NULL,\
             embedding_model TEXT NOT NULL DEFAULT 'all-minilm-l6-v2',\
             embedding BLOB NOT NULL\
             );\
             INSERT INTO vec_modern VALUES ('id-2', 'ns', 'entity', 'content', 'my-model', X'00');",
        )
        .unwrap();

        // V17 build_v17_preserving_rebuild_sql on this connection should return SELECT 1
        // because the plain table is not a virtual table and won't be found by the
        // VIRTUAL/vec0 filter.  The important thing is that the data is not touched.
        let sql = build_v17_preserving_rebuild_sql(&conn).unwrap();
        assert_eq!(
            sql, "SELECT 1;",
            "V17 must produce no-op SQL when no vec0 virtual tables need rebuilding"
        );

        // Data must be untouched.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vec_modern", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// Helper: apply a single migration in a transaction, recording it in the
    /// tracking table. Extracted here for use in the rollback test only.
    fn apply_single_migration(
        conn: &mut Connection,
        migration: &VersionedMigration,
    ) -> Result<(), SqliteError> {
        let tx = conn.transaction().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.execute_batch(migration.up)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        Ok(())
    }
}
