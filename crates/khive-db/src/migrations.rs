//! Schema migration system for the SQLite storage layer.
//!
//! Two APIs coexist:
//! - **Legacy per-service migrations** (`ServiceSchemaPlan` / `apply_schema_plan`):
//!   used by pack-scoped schemas.
//! - **Versioned migrations** (`MIGRATIONS` / `run_migrations`): the forward-only
//!   migration pipeline for the core tables.

use rusqlite::Connection;

use crate::error::SqliteError;

// =============================================================================
// Legacy per-service migration API (preserved for backward compatibility)
// =============================================================================

/// A single legacy migration step within a `ServiceSchemaPlan`.
pub struct Migration {
    /// Unique identifier for this migration.
    pub id: &'static str,
    /// SQL to apply (forward direction).
    pub up_sql: &'static str,
    /// SQL to revert (optional).
    pub down_sql: Option<&'static str>,
    /// Optional predicate: returns true if migration was already applied
    /// through a mechanism other than the migration tracker.
    pub is_already_applied: Option<fn(&Connection) -> bool>,
}

/// A pack-scoped schema plan containing migrations for SQLite and Postgres.
pub struct ServiceSchemaPlan {
    /// Service name used as a key in the `_schema_versions` tracking table.
    pub service: &'static str,
    /// SQLite-specific migration steps, applied in order.
    pub sqlite: &'static [Migration],
    /// Postgres-specific migration steps (reserved for future use).
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

/// Apply a pack-scoped schema plan, tracking each migration in `_schema_versions`.
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
/// Append a `VersionedMigration` with `version = <last + 1>`. The sequence
/// must be contiguous; `run_migrations` returns an error on gaps.
///
/// V2/V5 add columns that may already exist from in-process DDL -- the
/// runner checks column existence before applying. V4 deduplicates
/// graph_edges triples. V9 rebuilds graph_edges for lifecycle columns.
/// V13 event observability SQL is computed at runtime to avoid
/// duplicate-column errors on pre-bootstrapped DBs.
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

/// V17: sqlite-vec preserving rebuild.
///
/// Unlike V16 (regular tables), vec0 virtual tables cannot `ALTER TABLE ADD
/// COLUMN`. V17 does a 6-step copy-with-default rebuild per table: create
/// temp regular table, copy rows with defaults, drop virtual table,
/// recreate with full schema, copy back, drop temp. SQL is computed at
/// runtime via `build_v17_preserving_rebuild_sql`. After V17, all vec0
/// tables have `field` and `embedding_model`.
const V17_VECTOR_EMBEDDING_MODEL_TAG_PRESERVING_REBUILD: &str = "__v17_computed_at_runtime__";

/// V15: proposals_open projection table.
///
/// Maintains a fold-derived view of the four proposal EventKinds so that
/// `list(kind=proposal, status="open")` is an index scan rather than a full
/// event-log fold.
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

/// All versioned migrations in ascending order, applied by `run_migrations`.
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

/// Apply all unapplied migrations in order. Idempotent; each migration runs in its own transaction.
/// Errors on non-contiguous version array or failed migration.
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
/// 1. `CREATE TABLE tmp_vec_<engine>` — plain regular table with all columns
/// 2. `INSERT INTO tmp_vec_<engine> SELECT` — copies existing rows, backfilling
///    missing `field` to `''` and `embedding_model` to the inferred model name
/// 3. `DROP TABLE vec_<engine>` — removes the old virtual table
/// 4. `CREATE VIRTUAL TABLE vec_<engine> USING vec0(...)` — recreates with full schema
/// 5. `INSERT INTO vec_<engine> SELECT FROM tmp_vec_<engine>`
/// 6. `DROP TABLE tmp_vec_<engine>`
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
    /// Vector engine name (e.g. `"paraphrase"`).
    pub engine_name: String,
    /// Model identifier (e.g. `"all-minilm-l6-v2"`).
    pub model_id: String,
    /// Canonical deduplication key combining engine and model.
    pub key_version: String,
    /// Embedding dimensionality.
    pub dimensions: u32,
    /// Lifecycle status (`"active"` or `"superseded"`).
    pub status: String,
    /// Epoch timestamp when the model was activated.
    pub activated_at: Option<i64>,
    /// Epoch timestamp when the model was superseded.
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

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
