//! Schema migration system for the SQLite storage layer.
//!
//! Two APIs coexist:
//! - **Legacy per-service migrations** (`ServiceSchemaPlan` / `apply_schema_plan`):
//!   used by pack-scoped schemas.
//! - **Versioned migrations** (`MIGRATIONS` / `run_migrations`): the forward-only
//!   migration pipeline for the core tables.

use khive_storage::blob::ContentRef;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;

use crate::error::SqliteError;
use crate::stores::blob::{try_acquire_database_gc_owner_for_path, DatabaseGcOwnerGuard};

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

const SCHEMA_VERSION_TABLE: &str = include_str!("../sql/schema-version-table.sql");

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

        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(migration.up_sql)?;

        tx.execute(
            "INSERT INTO _schema_versions (service, migration_id, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                plan.service,
                migration.id,
                chrono::Utc::now().timestamp_micros(),
            ],
        )?;
        tx.commit()?;
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

// V1: complete schema, loaded from sql/schema.sql.
// Fresh-start repo (v0.2.8) — all schema in one migration, no incremental versions.
const V1_UP: &str = include_str!("../sql/schema.sql");

const V2_UP: &str = include_str!("../sql/002-narrow-fts-sections-update-trigger.sql");

const V3_UP: &str = include_str!("../sql/003-backfill-domain-mirror-atoms.sql");

const V4_UP: &str = include_str!("../sql/004-fts-consolidation.sql");

const V5_UP: &str = include_str!("../sql/005-unique-comm-external-id.sql");

const V6_UP: &str = include_str!("../sql/006-brain-retune-driver.sql");

const V7_UP: &str = include_str!("../sql/007-notes-seq.sql");

const V8_UP: &str = include_str!("../sql/008-notes-seq-repair.sql");

const V9_UP: &str = include_str!("../sql/009-entities-name-ci-index.sql");

const V10_UP: &str = include_str!("../sql/010-entities-content-ref.sql");

const V11_UP: &str = include_str!("../sql/011-ann-write-log.sql");

const V12_UP: &str = include_str!("../sql/012-ann-write-log-model-seq-index.sql");

const V13_UP: &str = include_str!("../sql/013-list-cursor-sequences.sql");

const V14_UP: &str = include_str!("../sql/014-graph-edges-id-unique.sql");

const V15_UP: &str = include_str!("../sql/015-serve-ledger-attribution.sql");

const V16_UP: &str = include_str!("../sql/016-gtd-dependency-cycle-guards.sql");

const V17_UP: &str = include_str!("../sql/017-agents-ddl.sql");

const V18_UP: &str = include_str!("../sql/018-ann-consumer-pending.sql");

const V19_UP: &str = include_str!("../sql/019-list-cursor-backfill-repair.sql");

const V20_UP: &str = include_str!("../sql/020-blob-gc-claims.sql");

const V21_STAGE_UP: &str = include_str!("../sql/021-attachments-a-stage.sql");

const V21_ATTACHMENT_FENCES_UP: &str = include_str!("../sql/021-attachments-b-claim-fences.sql");

/// Core schema version reserved for ADR-121's attachments-first cutover.
pub const ATTACHMENT_CUTOVER_VERSION: u32 = 21;

/// The latest schema version this build's migration chain produces.
///
/// Terminal-version assertions belong on this, not on a hardcoded number:
/// a literal decays into a wrong claim the next time a migration is added.
pub fn latest_schema_version() -> u32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// DDL for the `ann_write_log` delta table.
///
/// Shared between migration V11 and the belt-and-suspenders creation in
/// `StorageBackend::vectors_for_namespace` (same pattern as
/// [`EMBEDDING_MODELS_DDL`]): every database that hosts `vec_*` tables must
/// also have the write log, or vector writes would fail on databases opened
/// without `run_migrations()`. The `.sql` file is `IF NOT EXISTS`-idempotent.
pub const ANN_WRITE_LOG_DDL: &str = V11_UP;

/// DDL for the `ann_write_log` model/kind/field-leading index (ADR-118 §"Cost
/// bound"), shared between migration V12 and the belt-and-suspenders creation
/// in `StorageBackend::vectors_for_namespace` for the same reason as
/// [`ANN_WRITE_LOG_DDL`].
pub const ANN_WRITE_LOG_MODEL_SEQ_INDEX_DDL: &str = V12_UP;

/// Idempotent DDL for pending ANN-consumer lifecycle metadata (#1479).
///
/// The V18 migration additionally translates legacy zero-watermark rows once.
/// This constant deliberately contains only idempotent DDL: vector-store open
/// paths may execute it repeatedly and must never demote a valid active
/// checkpoint at sequence zero back to pending.
pub const ANN_CONSUMER_PENDING_DDL: &str = include_str!("../sql/ann-consumer-pending-ddl.sql");

/// DDL for the `_embedding_models` registry table.
///
/// Shared between the V1 schema and the belt-and-suspenders creation in
/// `StorageBackend::vectors_for_namespace`. Both sites reference this constant so
/// the schema cannot silently diverge if the registry evolves.
pub const EMBEDDING_MODELS_DDL: &str = include_str!("../sql/embedding-models-ddl.sql");

/// Canonical versioned migration ledger in ascending order.
///
/// [`run_migrations`] applies the ordinary prefix and may complete V21 through
/// its zero-legacy-reference fast path. A legacy V20 database records V21 only
/// when [`finalize_attachment_cutover`] commits the application-assisted
/// cutover.
pub const MIGRATIONS: &[VersionedMigration] = &[
    VersionedMigration {
        version: 1,
        name: "initial_schema",
        up: V1_UP,
    },
    VersionedMigration {
        version: 2,
        name: "narrow_fts_sections_update_trigger",
        up: V2_UP,
    },
    VersionedMigration {
        version: 3,
        name: "backfill_domain_mirror_atoms",
        up: V3_UP,
    },
    VersionedMigration {
        version: 4,
        name: "fts_consolidation",
        up: V4_UP,
    },
    VersionedMigration {
        version: 5,
        name: "unique_comm_message_external_id",
        up: V5_UP,
    },
    VersionedMigration {
        version: 6,
        name: "brain_retune_driver",
        up: V6_UP,
    },
    VersionedMigration {
        version: 7,
        name: "notes_seq",
        up: V7_UP,
    },
    VersionedMigration {
        version: 8,
        name: "notes_seq_repair",
        up: V8_UP,
    },
    VersionedMigration {
        version: 9,
        name: "entities_name_ci_index",
        up: V9_UP,
    },
    VersionedMigration {
        version: 10,
        name: "entities_content_ref",
        up: V10_UP,
    },
    VersionedMigration {
        version: 11,
        name: "ann_write_log",
        up: V11_UP,
    },
    VersionedMigration {
        version: 12,
        name: "ann_write_log_model_seq_index",
        up: V12_UP,
    },
    VersionedMigration {
        version: 13,
        name: "list_cursor_sequences",
        up: V13_UP,
    },
    VersionedMigration {
        version: 14,
        name: "graph_edges_id_unique",
        up: V14_UP,
    },
    VersionedMigration {
        version: 15,
        name: "serve_ledger_attribution",
        up: V15_UP,
    },
    VersionedMigration {
        version: 16,
        name: "gtd_dependency_cycle_guards",
        up: V16_UP,
    },
    VersionedMigration {
        version: 17,
        name: "agents_ddl",
        up: V17_UP,
    },
    VersionedMigration {
        version: 18,
        name: "ann_consumer_pending",
        up: V18_UP,
    },
    VersionedMigration {
        version: 19,
        name: "list_cursor_backfill_repair",
        up: V19_UP,
    },
    VersionedMigration {
        version: 20,
        name: "blob_gc_claims",
        up: V20_UP,
    },
    VersionedMigration {
        version: ATTACHMENT_CUTOVER_VERSION,
        name: "attachments_first_class",
        // V21 is coordinated rather than an unconditional SQL migration.
        // The runner special-cases it below; exposing the stage DDL here keeps
        // the ledger entry self-describing for migration inspection tooling.
        up: V21_STAGE_UP,
    },
];

/// Durable state of ADR-121's boot-gated, two-stage attachment cutover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentCutoverStatus {
    /// V20 is current and no stage marker has been committed.
    Pending,
    /// Stage 1 committed; boot must finish verified pack-owned attachments.
    Incomplete,
    /// V21, the attachment fences, and the attachment-only schema committed.
    Complete,
}

fn schema_object_exists(
    conn: &Connection,
    object_type: &str,
    name: &str,
) -> Result<bool, SqliteError> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = ?1 AND name = ?2",
        rusqlite::params![object_type, name],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn schema_column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, SqliteError> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn require_attachment_schema_objects(
    conn: &Connection,
    objects: &[(&str, &str)],
    phase: &str,
) -> Result<(), SqliteError> {
    for (object_type, name) in objects {
        if !schema_object_exists(conn, object_type, name)? {
            return Err(SqliteError::InvalidData(format!(
                "attachment cutover {phase} state is missing {object_type} {name:?}"
            )));
        }
    }
    Ok(())
}

fn validate_incomplete_attachment_schema(conn: &Connection) -> Result<(), SqliteError> {
    require_attachment_schema_objects(
        conn,
        &[
            ("table", "attachments"),
            ("index", "idx_attachments_content_ref"),
        ],
        "incomplete",
    )?;
    require_legacy_attachment_fences(conn)
}

fn validate_complete_attachment_schema(conn: &Connection) -> Result<(), SqliteError> {
    require_attachment_schema_objects(
        conn,
        &[
            ("table", "attachments"),
            ("table", "blob_gc_claims"),
            ("index", "idx_attachments_content_ref"),
            ("index", "idx_blob_gc_claims_content_ref"),
            ("trigger", "attachments_reject_claimed_blob_insert"),
            ("trigger", "attachments_reject_claimed_blob_update"),
        ],
        "complete",
    )?;
    if schema_column_exists(conn, "entities", "content_ref")? {
        return Err(SqliteError::InvalidData(
            "attachment cutover is complete but entities.content_ref still exists".into(),
        ));
    }
    for (object_type, name) in [
        ("index", "idx_entities_content_ref"),
        ("trigger", "entities_reject_claimed_blob_insert"),
        ("trigger", "entities_reject_claimed_blob_update"),
    ] {
        if schema_object_exists(conn, object_type, name)? {
            return Err(SqliteError::InvalidData(format!(
                "attachment cutover is complete but legacy {object_type} {name:?} still exists"
            )));
        }
    }
    Ok(())
}

/// Inspect the coordinated V21 state without mutating the connection.
///
/// The marker and migration ledger form one state machine. Impossible pairs
/// fail closed instead of being guessed into a resumable state.
pub fn attachment_cutover_status(
    conn: &Connection,
) -> Result<AttachmentCutoverStatus, SqliteError> {
    let version = read_schema_version(conn)?;
    let marker_table = schema_object_exists(conn, "table", "attachment_cutover_state")?;
    if !marker_table {
        if version >= ATTACHMENT_CUTOVER_VERSION {
            return Err(SqliteError::InvalidData(format!(
                "migration V{ATTACHMENT_CUTOVER_VERSION} is recorded but its attachment cutover marker is absent"
            )));
        }
        if schema_object_exists(conn, "table", "attachments")? {
            return Err(SqliteError::InvalidData(
                "attachments table exists without the durable attachment cutover marker".into(),
            ));
        }
        return Ok(AttachmentCutoverStatus::Pending);
    }

    let marker: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT state, completed_at FROM attachment_cutover_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match marker {
        Some((state, None)) if state == "incomplete" => {
            if version >= ATTACHMENT_CUTOVER_VERSION {
                Err(SqliteError::InvalidData(format!(
                    "attachment cutover is incomplete but migration V{ATTACHMENT_CUTOVER_VERSION} is already recorded"
                )))
            } else {
                validate_incomplete_attachment_schema(conn)?;
                Ok(AttachmentCutoverStatus::Incomplete)
            }
        }
        Some((state, Some(_))) if state == "complete" => {
            // Later migrations (V22+) are recorded on top of a completed
            // cutover in the normal course; only a ledger BELOW V21 beside a
            // complete marker is an impossible pair.
            if version >= ATTACHMENT_CUTOVER_VERSION {
                validate_complete_attachment_schema(conn)?;
                Ok(AttachmentCutoverStatus::Complete)
            } else {
                Err(SqliteError::InvalidData(format!(
                    "attachment cutover is complete but schema ledger is at V{version}, below V{ATTACHMENT_CUTOVER_VERSION}"
                )))
            }
        }
        Some((state, completed_at)) => Err(SqliteError::InvalidData(format!(
            "invalid attachment cutover marker state {state:?} with completed_at={completed_at:?}"
        ))),
        None => Err(SqliteError::InvalidData(
            "attachment cutover marker table exists without its singleton row".into(),
        )),
    }
}

fn require_legacy_attachment_fences(conn: &Connection) -> Result<(), SqliteError> {
    if !schema_column_exists(conn, "entities", "content_ref")? {
        return Err(SqliteError::InvalidData(
            "attachment cutover requires legacy entities.content_ref until finalization".into(),
        ));
    }
    for (object_type, name) in [
        ("table", "blob_gc_claims"),
        ("index", "idx_blob_gc_claims_content_ref"),
        ("index", "idx_entities_content_ref"),
        ("trigger", "entities_reject_claimed_blob_insert"),
        ("trigger", "entities_reject_claimed_blob_update"),
    ] {
        if !schema_object_exists(conn, object_type, name)? {
            return Err(SqliteError::InvalidData(format!(
                "attachment cutover requires legacy {object_type} {name:?} until finalization"
            )));
        }
    }
    Ok(())
}

// length() and GLOB both stop scanning at an embedded NUL, so a value of 64
// hex characters followed by a NUL and arbitrary trailing bytes would pass
// both. Deriving the canonical byte width from the connection's own text
// encoding (rather than assuming UTF-8) keeps this arm correct on a database
// pinned to UTF-16 and fails closed if the probe returns something else,
// matching the pattern already used by `validate_blob_gc_evidence`.
fn canonical_content_ref_byte_width(conn: &Connection) -> Result<i64, SqliteError> {
    let width: i64 = conn.query_row("SELECT length(CAST('x' AS BLOB))", [], |row| row.get(0))?;
    if !(1..=4).contains(&width) {
        return Err(SqliteError::InvalidData(format!(
            "the text-encoding width probe returned {width}; refusing canonicality validation"
        )));
    }
    Ok(width * 64)
}

fn validate_canonical_legacy_refs(conn: &Connection) -> Result<(), SqliteError> {
    let canonical_bytes = canonical_content_ref_byte_width(conn)?;
    let invalid: Option<String> = conn
        .query_row(
            "SELECT id FROM entities \
             WHERE content_ref IS NOT NULL \
               AND (typeof(content_ref) <> 'text' \
                 OR length(content_ref) <> 64 \
                 OR length(CAST(content_ref AS BLOB)) <> ?1 \
                 OR content_ref GLOB '*[^0-9a-f]*') \
             LIMIT 1",
            [canonical_bytes],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = invalid {
        return Err(SqliteError::InvalidData(format!(
            "entities.content_ref for record {id:?} is not a canonical 64-character lowercase hexadecimal ContentRef"
        )));
    }
    Ok(())
}

fn validate_canonical_attachment_and_claim_refs(conn: &Connection) -> Result<(), SqliteError> {
    let canonical_bytes = canonical_content_ref_byte_width(conn)?;
    for (table, identity) in [
        ("attachments", "record_uuid"),
        ("blob_gc_claims", "root_key"),
    ] {
        let sql = format!(
            "SELECT {identity} FROM {table} \
             WHERE typeof(content_ref) <> 'text' \
                OR length(content_ref) <> 64 \
                OR length(CAST(content_ref AS BLOB)) <> ?1 \
                OR content_ref GLOB '*[^0-9a-f]*' \
             LIMIT 1"
        );
        let invalid: Option<String> = conn
            .query_row(&sql, [canonical_bytes], |row| row.get(0))
            .optional()?;
        if let Some(owner) = invalid {
            return Err(SqliteError::InvalidData(format!(
                "{table}.content_ref for {identity} {owner:?} is not canonical"
            )));
        }
    }
    Ok(())
}

fn validate_attachment_record_owners(conn: &Connection) -> Result<(), SqliteError> {
    let dangling: Option<(String, String)> = conn
        .query_row(
            "SELECT record_uuid, substrate FROM attachments AS attachment \
             WHERE (substrate = 'entity' AND NOT EXISTS ( \
                       SELECT 1 FROM entities WHERE id = attachment.record_uuid \
                   )) \
                OR (substrate = 'note' AND NOT EXISTS ( \
                       SELECT 1 FROM notes WHERE id = attachment.record_uuid \
                   )) \
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((record_uuid, substrate)) = dangling {
        return Err(SqliteError::InvalidData(format!(
            "attachment role references absent {substrate} record {record_uuid:?}"
        )));
    }
    Ok(())
}

fn validate_legacy_content_backfill(conn: &Connection) -> Result<(), SqliteError> {
    let conflict: Option<String> = conn
        .query_row(
            "SELECT entity.id FROM entities AS entity \
             LEFT JOIN attachments AS attachment \
               ON attachment.record_uuid = entity.id AND attachment.role = 'content' \
             WHERE entity.content_ref IS NOT NULL \
               AND (attachment.record_uuid IS NULL \
                 OR attachment.substrate <> 'entity' \
                 OR attachment.content_ref <> entity.content_ref) \
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(record_uuid) = conflict {
        return Err(SqliteError::InvalidData(format!(
            "legacy content attachment for entity {record_uuid:?} is missing or conflicts with entities.content_ref"
        )));
    }
    Ok(())
}

fn stage_attachment_cutover_on_connection(conn: &Connection, now: i64) -> Result<(), SqliteError> {
    require_legacy_attachment_fences(conn)?;
    conn.execute_batch(V21_STAGE_UP)?;
    validate_canonical_legacy_refs(conn)?;
    validate_canonical_attachment_and_claim_refs(conn)?;

    let conflict: Option<String> = conn
        .query_row(
            "SELECT entity.id FROM entities AS entity \
             JOIN attachments AS attachment \
               ON attachment.record_uuid = entity.id AND attachment.role = 'content' \
             WHERE entity.content_ref IS NOT NULL \
               AND (attachment.substrate <> 'entity' \
                 OR attachment.content_ref <> entity.content_ref) \
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(record_uuid) = conflict {
        return Err(SqliteError::InvalidData(format!(
            "existing content attachment for entity {record_uuid:?} conflicts with entities.content_ref"
        )));
    }

    conn.execute(
        "INSERT INTO attachments \
         (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) \
         SELECT id, 'entity', 'content', content_ref, NULL, NULL, created_at \
         FROM entities WHERE content_ref IS NOT NULL \
         ON CONFLICT(record_uuid, role) DO NOTHING",
        [],
    )?;
    validate_legacy_content_backfill(conn)?;

    // The caller holds the canonical database GC owner, so every preexisting
    // claim is abandoned. Clearing happens before the durable incomplete
    // marker is exposed and remains inside this one transaction.
    conn.execute("DELETE FROM blob_gc_claims", [])?;
    conn.execute(
        "INSERT INTO attachment_cutover_state \
         (singleton, state, started_at, completed_at) \
         VALUES (1, 'incomplete', ?1, NULL) \
         ON CONFLICT(singleton) DO NOTHING",
        [now],
    )?;
    Ok(())
}

/// Commit stage 1 of the coordinated V21 migration.
///
/// The caller must hold [`crate::stores::blob::DatabaseGcOwnerGuard`] for the
/// canonical database before entering this function and retain it through
/// application backfill and finalization. This function owns one IMMEDIATE
/// SQLite transaction; a failure leaves neither its DDL nor marker visible.
pub fn stage_attachment_cutover(conn: &mut Connection) -> Result<(), SqliteError> {
    match attachment_cutover_status(conn)? {
        AttachmentCutoverStatus::Complete => return Ok(()),
        AttachmentCutoverStatus::Pending | AttachmentCutoverStatus::Incomplete => {}
    }
    if read_schema_version(conn)? != ATTACHMENT_CUTOVER_VERSION - 1 {
        return Err(SqliteError::InvalidData(format!(
            "attachment cutover stage requires canonical V{} schema",
            ATTACHMENT_CUTOVER_VERSION - 1
        )));
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let status = attachment_cutover_status(&tx)?;
    if status == AttachmentCutoverStatus::Complete {
        return Ok(());
    }
    stage_attachment_cutover_on_connection(&tx, chrono::Utc::now().timestamp_micros())?;
    tx.commit()?;
    Ok(())
}

/// Add one host-verified pack-owned attachment during V21 stage 2.
///
/// This helper is deliberately transaction-neutral: it neither begins nor
/// commits a transaction. The boot coordinator can therefore apply the full
/// verified vector in one caller-owned IMMEDIATE transaction while retaining
/// the canonical database GC owner. Reapplying the same role and content is
/// idempotent; a different substrate or digest for that role fails closed.
#[allow(clippy::too_many_arguments)]
pub fn apply_generic_verified_attachment(
    conn: &Connection,
    record_uuid: &str,
    substrate: &str,
    role: &str,
    content_ref: &ContentRef,
    media_type: Option<&str>,
    size_bytes: Option<u64>,
    created_at: i64,
) -> Result<(), SqliteError> {
    if attachment_cutover_status(conn)? != AttachmentCutoverStatus::Incomplete {
        return Err(SqliteError::InvalidData(
            "verified application attachments may only be applied while V21 cutover is incomplete"
                .into(),
        ));
    }
    if role.is_empty() || role.chars().any(char::is_control) {
        return Err(SqliteError::InvalidData(
            "attachment role must be non-empty and contain no control characters".into(),
        ));
    }
    let size_bytes = size_bytes.map(i64::try_from).transpose().map_err(|_| {
        SqliteError::InvalidData("attachment size_bytes exceeds SQLite INTEGER".into())
    })?;
    let owner_table = match substrate {
        "entity" => "entities",
        "note" => "notes",
        other => {
            return Err(SqliteError::InvalidData(format!(
                "attachment substrate must be 'entity' or 'note', got {other:?}"
            )))
        }
    };
    let owner_sql = format!("SELECT COUNT(*) > 0 FROM {owner_table} WHERE id = ?1");
    let owner_exists: bool = conn.query_row(&owner_sql, [record_uuid], |row| row.get(0))?;
    if !owner_exists {
        return Err(SqliteError::InvalidData(format!(
            "cannot attach role {role:?}: {substrate} record {record_uuid:?} does not exist"
        )));
    }
    let claimed: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM blob_gc_claims WHERE content_ref = ?1",
        [content_ref.as_str()],
        |row| row.get(0),
    )?;
    if claimed {
        return Err(SqliteError::InvalidData(format!(
            "cannot attach claimed content_ref {} during V21 cutover",
            content_ref.as_str()
        )));
    }

    let changed = conn.execute(
        "INSERT INTO attachments \
         (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(record_uuid, role) DO UPDATE SET \
             media_type = excluded.media_type, \
             size_bytes = excluded.size_bytes, \
             created_at = excluded.created_at \
         WHERE attachments.substrate = excluded.substrate \
           AND attachments.content_ref = excluded.content_ref",
        rusqlite::params![
            record_uuid,
            substrate,
            role,
            content_ref.as_str(),
            media_type,
            size_bytes,
            created_at,
        ],
    )?;
    if changed == 0 {
        return Err(SqliteError::InvalidData(format!(
            "attachment role {role:?} for record {record_uuid:?} conflicts with an existing substrate or content_ref"
        )));
    }
    Ok(())
}

fn finalize_attachment_cutover_on_connection(
    conn: &Connection,
    now: i64,
) -> Result<(), SqliteError> {
    require_legacy_attachment_fences(conn)?;
    validate_canonical_legacy_refs(conn)?;
    validate_canonical_attachment_and_claim_refs(conn)?;
    validate_attachment_record_owners(conn)?;
    validate_legacy_content_backfill(conn)?;

    let remaining_claims: i64 =
        conn.query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))?;
    if remaining_claims != 0 {
        return Err(SqliteError::InvalidData(format!(
            "attachment cutover cannot finalize while {remaining_claims} blob GC claim rows remain"
        )));
    }

    let uncovered_model: Option<String> = conn
        .query_row(
            "SELECT model.id FROM entities AS model \
             WHERE model.entity_type = 'moodboard_model' \
               AND model.content_ref IS NOT NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM attachments AS attachment \
                   WHERE attachment.record_uuid = model.id \
                     AND attachment.substrate = 'entity' \
                     AND attachment.role = 'fann-network' \
               ) \
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(record_uuid) = uncovered_model {
        return Err(SqliteError::InvalidData(format!(
            "moodboard_model {record_uuid:?} has legacy content but no verified 'fann-network' attachment"
        )));
    }

    conn.execute_batch(V21_ATTACHMENT_FENCES_UP)?;
    conn.execute_batch(
        "DROP TRIGGER entities_reject_claimed_blob_insert; \
         DROP TRIGGER entities_reject_claimed_blob_update; \
         DROP INDEX idx_entities_content_ref; \
         ALTER TABLE entities DROP COLUMN content_ref;",
    )?;
    conn.execute(
        "UPDATE attachment_cutover_state \
         SET state = 'complete', completed_at = ?1 \
         WHERE singleton = 1 AND state = 'incomplete'",
        [now],
    )?;
    Ok(())
}

fn record_attachment_cutover_migration(conn: &Connection, now: i64) -> Result<(), SqliteError> {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.version == ATTACHMENT_CUTOVER_VERSION)
        .expect("V21 migration must be registered");
    conn.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![migration.version, migration.name, now],
    )?;
    Ok(())
}

/// Atomically switch an explicitly staged database to attachment-only V21.
///
/// The caller must still hold the canonical database GC owner. The exclusive
/// transition revalidates every legacy and attachment reference, verifies
/// moodboard model role coverage, replaces the claim fences, removes the old
/// column, marks the cutover complete, and records V21 in one transaction.
pub fn finalize_attachment_cutover(conn: &mut Connection) -> Result<(), SqliteError> {
    if attachment_cutover_status(conn)? == AttachmentCutoverStatus::Complete {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;
    match attachment_cutover_status(&tx)? {
        AttachmentCutoverStatus::Complete => return Ok(()),
        AttachmentCutoverStatus::Pending => {
            return Err(SqliteError::InvalidData(
                "attachment cutover must complete stage 1 before finalization".into(),
            ))
        }
        AttachmentCutoverStatus::Incomplete => {}
    }
    let now = chrono::Utc::now().timestamp_micros();
    finalize_attachment_cutover_on_connection(&tx, now)?;
    record_attachment_cutover_migration(&tx, now)?;
    tx.commit()?;
    Ok(())
}

/// Read the ordered migration ledger prefix without interpreting its rows.
fn read_applied_migration_ledger(
    conn: &Connection,
    through_version: u32,
) -> Result<Vec<(u32, String)>, SqliteError> {
    let mut stmt = conn.prepare(
        "SELECT version, name FROM _schema_migrations \
         WHERE version <= ?1 ORDER BY version ASC",
    )?;
    let rows = stmt
        .query_map([through_version], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Require the applied versions through `through_version` to be the exact
/// contiguous canonical prefix of [`MIGRATIONS`]. A matching `MAX(version)` is
/// insufficient: a missing middle row or foreign version can expose a
/// materially different schema while retaining the same maximum.
fn validate_applied_migration_versions(
    applied: &[(u32, String)],
    through_version: u32,
) -> Result<(), SqliteError> {
    let expected: Vec<&VersionedMigration> = MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= through_version)
        .collect();
    let mut applied_index = 0;

    for migration in expected {
        let Some((version, applied_name)) = applied.get(applied_index) else {
            return Err(SqliteError::InvalidData(format!(
                "migration history is missing version {} ('{}'); the applied ledger must be \
                 the exact contiguous canonical sequence through version {through_version}",
                migration.version, migration.name,
            )));
        };
        if *version < migration.version {
            return Err(SqliteError::InvalidData(format!(
                "migration history contains unknown version {version} recorded as \
                 '{applied_name}'; the applied ledger must contain only canonical versions"
            )));
        }
        if *version > migration.version {
            return Err(SqliteError::InvalidData(format!(
                "migration history is missing version {} ('{}'); found version {version} \
                 next instead",
                migration.version, migration.name,
            )));
        }
        applied_index += 1;
    }

    if let Some((version, name)) = applied.get(applied_index) {
        return Err(SqliteError::InvalidData(format!(
            "migration history contains unknown version {version} recorded as '{name}'; \
             the applied ledger must contain only canonical versions"
        )));
    }

    Ok(())
}

fn validate_applied_migration_names(
    applied: &[(u32, String)],
    through_version: u32,
    allow_known_v19_repairs: bool,
) -> Result<(), SqliteError> {
    for ((version, applied_name), migration) in applied.iter().zip(
        MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= through_version),
    ) {
        debug_assert_eq!(*version, migration.version);
        if migration.name != applied_name.as_str() {
            if allow_known_v19_repairs && matches!(*version, 13 | 14) {
                continue;
            }
            return Err(SqliteError::InvalidData(format!(
                "migration version {version} is recorded under name '{applied_name}', \
                 expected '{expected}'. This database's migration history does not match \
                 the current binary; recreate it from the current schema or repair the \
                 specific known divergence via a dedicated migration.",
                expected = migration.name,
            )));
        }
    }

    Ok(())
}

/// Confirm the complete applied ledger is the canonical prefix, including
/// names. The only historical V13/V14 name divergence is repaired by V19
/// before this validator runs for a pre-V19 database.
fn validate_applied_migration_ledger(
    conn: &Connection,
    through_version: u32,
) -> Result<(), SqliteError> {
    let applied = read_applied_migration_ledger(conn, through_version)?;
    validate_applied_migration_versions(&applied, through_version)?;
    validate_applied_migration_names(&applied, through_version, false)
}

const MIGRATION_TRACKING_TABLE: &str = include_str!("../sql/schema-migrations-table.sql");

/// Read the applied schema version from an open connection **without** running
/// migrations. Returns 0 when the `_schema_migrations` ledger is absent (an
/// un-migrated or empty database); any other failure (BUSY, IO) propagates —
/// collapsing it to 0 would misreport a live database as un-migrated. Never
/// writes.
pub fn read_schema_version(conn: &Connection) -> Result<u32, SqliteError> {
    match conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
        [],
        |row| row.get(0),
    ) {
        Ok(version) => Ok(version),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
            if msg.contains("no such table: _schema_migrations") =>
        {
            Ok(0)
        }
        Err(e) => Err(e.into()),
    }
}

/// Open `path` read-only and report its applied schema version without creating
/// or migrating the file. The caller must ensure `path` exists — opening a
/// missing file read-only errors rather than creating it. This is the path used
/// by schema-inspection commands that must not mutate the database.
pub fn inspect_schema_version(path: &std::path::Path) -> Result<u32, SqliteError> {
    let conn = crate::pool::open_read_only_snapshot_connection(path)?;
    read_schema_version(&conn)
}

/// Open `path` read-only and require the exact canonical current ledger and
/// physical cutover state, without creating files or applying migrations.
pub fn inspect_schema_is_current(path: &std::path::Path) -> Result<u32, SqliteError> {
    let conn = crate::pool::open_read_only_snapshot_connection(path)?;
    validate_schema_is_current(&conn)
}

/// Require an already-open database to match this build's latest core schema
/// without applying migrations.
///
/// A read-only snapshot behind the current migration set cannot be repaired in
/// place, while a snapshot ahead of the binary may contain schema this build
/// does not understand. Both directions fail with an actionable diagnostic; an
/// exact match performs no writes.
pub fn validate_schema_is_current(conn: &Connection) -> Result<u32, SqliteError> {
    let current_version = read_schema_version(conn)?;
    let latest_version = latest_schema_version();

    if current_version < latest_version {
        return Err(SqliteError::InvalidData(format!(
            "read-only database schema version {current_version} is behind the latest known \
             migration {latest_version}; migrate a writable copy with this build before opening \
             the snapshot read-only"
        )));
    }
    if current_version > latest_version {
        return Err(SqliteError::InvalidData(format!(
            "read-only database schema version {current_version} is ahead of the latest known \
             migration {latest_version}; use a compatible newer build or recreate the snapshot"
        )));
    }

    // Numeric equality alone is not enough: a database can carry the current
    // maximum version under renamed or foreign migration ledger entries while
    // exposing a materially different schema. Writable boot runs this same
    // closed-name validation in `run_migrations_locked`; snapshot inspection
    // must not accept a history that ordinary boot would reject merely because
    // it cannot repair it in place.
    validate_applied_migration_ledger(conn, current_version)?;
    // `>=`, not `==`: later migrations (V22+) record on top of a completed
    // cutover, and a ledger at the latest version must not exempt the
    // physical cutover state from validation.
    if current_version >= ATTACHMENT_CUTOVER_VERSION
        && attachment_cutover_status(conn)? != AttachmentCutoverStatus::Complete
    {
        return Err(SqliteError::InvalidData(
            "read-only database has not completed the V21 attachment cutover".into(),
        ));
    }

    Ok(current_version)
}

#[cfg(test)]
pub(crate) mod test_sync {
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Barrier, Mutex};

    /// When set, `run_migrations_locked` parks after its initial (stale)
    /// ledger read until every racing thread has arrived — forcing the
    /// contended interleaving the concurrent-boot test asserts on.
    pub(crate) static STALE_READ_BARRIER: Mutex<Option<Arc<Barrier>>> = Mutex::new(None);
    /// Counts entries into the under-lock sibling fast-forward branch.
    pub(crate) static LOCKED_FAST_FORWARDS: AtomicU32 = AtomicU32::new(0);
    /// Set by the SQLite busy handler installed on participating connections:
    /// `true` means SQLite itself reported a blocked lock acquisition to the
    /// loser — actual contention, not merely an intended attempt.
    pub(crate) static BUSY_OBSERVED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Busy handler for participating test connections: records that SQLite
    /// observed a busy acquisition, then keeps retrying.
    pub(crate) fn record_busy(_count: i32) -> bool {
        BUSY_OBSERVED.store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(1));
        true
    }

    /// Set by the winner immediately before committing its first migration
    /// transaction — i.e. before the write lock is first released.
    pub(crate) static WINNER_COMMITTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    /// Recorded by the loser when its first `BEGIN IMMEDIATE` returns: whether
    /// the winner had already committed at that moment. `true` is direct
    /// evidence the loser's lock acquisition blocked across the winner's held
    /// write lock rather than the two calls serializing by scheduler accident.
    pub(crate) static LOSER_SAW_WINNER_COMMIT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    std::thread_local! {
        /// Opt-in flag: only threads that set this participate in the barrier,
        /// so unrelated tests migrating in parallel are never parked.
        pub(crate) static PARTICIPATE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
        /// Whether this thread has already instrumented its first BEGIN.
        pub(crate) static FIRST_BEGIN_DONE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }
}

/// Apply the ordinary unapplied migration prefix in order.
///
/// The operation is idempotent and each ordinary migration runs in its own
/// transaction. V21 is the application-assisted exception: a database with no
/// legacy content references may complete V21 atomically here, while a legacy
/// database stops successfully at V20 so the async host can stage, verify, and
/// finalize the attachment cutover. Errors on a non-contiguous migration array,
/// a non-canonical applied ledger, or a failed migration.
fn canonical_connection_database_path(conn: &Connection) -> Result<Option<PathBuf>, SqliteError> {
    let configured = conn.path().unwrap_or_default();
    let raw_path = if configured.is_empty() {
        conn.query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )?
    } else {
        configured.to_string()
    };

    if raw_path.is_empty() {
        return Ok(None);
    }
    std::fs::canonicalize(&raw_path)
        .map(Some)
        .map_err(SqliteError::Io)
}

fn validate_database_gc_owner(
    conn: &Connection,
    owner: &DatabaseGcOwnerGuard,
) -> Result<(), SqliteError> {
    let connection_path = canonical_connection_database_path(conn)?;
    if owner.database_path() != connection_path.as_deref() {
        return Err(SqliteError::InvalidData(format!(
            "database GC owner targets {:?}, but migration connection targets {:?}",
            owner.database_path(),
            connection_path.as_deref(),
        )));
    }
    Ok(())
}

pub fn run_migrations(conn: &mut Connection) -> Result<u32, SqliteError> {
    let database_path = canonical_connection_database_path(conn)?;
    if let Some(database_path) = database_path {
        // This raw API may have been handed a connection behind an opaque pool
        // writer guard. Never wait here and invert the canonical
        // owner-before-writer order; fail closed and direct production callers
        // to `StorageBackend::prepare_core_schema` instead.
        let owner = try_acquire_database_gc_owner_for_path(database_path).map_err(|error| {
            SqliteError::InvalidData(format!(
                "failed to acquire database GC owner before schema migration: {error}"
            ))
        })?;
        return run_migrations_with_database_gc_owner(conn, &owner);
    }

    // A raw in-memory connection has no durable/cross-process GC domain. The
    // production in-memory backend still uses the owner-aware path below.
    run_migrations_with_busy_timeout(conn)
}

pub(crate) fn run_migrations_with_database_gc_owner(
    conn: &mut Connection,
    owner: &DatabaseGcOwnerGuard,
) -> Result<u32, SqliteError> {
    validate_database_gc_owner(conn, owner)?;
    run_migrations_with_busy_timeout(conn)
}

fn run_migrations_with_busy_timeout(conn: &mut Connection) -> Result<u32, SqliteError> {
    // Concurrent boots (multiple processes migrating the same file) contend on
    // the write lock below; a short hot-path busy_timeout cannot wait out a
    // sibling's migration. Raise-only to a 5s floor — never reduce a caller
    // whose configured timeout is already longer — and restore after.
    let prior_busy_ms: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    let raised = prior_busy_ms < 5_000;
    if raised {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
    }
    let result = run_migrations_locked(conn);
    if raised {
        let _ = conn.busy_timeout(std::time::Duration::from_millis(prior_busy_ms.max(0) as u64));
    }
    result
}

fn run_migrations_locked(conn: &mut Connection) -> Result<u32, SqliteError> {
    conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

    let current_version: u32 = read_schema_version(conn)?;

    // Deterministic-contention hook: parks every caller after the stale ledger
    // read (no lock held) until all racing test threads have observed it, so
    // they are then released to compete for the IMMEDIATE write lock below.
    #[cfg(test)]
    if test_sync::PARTICIPATE.with(|p| p.get()) {
        // Replaces the busy_timeout raised by `run_migrations` on this test
        // connection: records SQLite-observed contention, then keeps retrying.
        conn.busy_handler(Some(test_sync::record_busy))?;
        let barrier = test_sync::STALE_READ_BARRIER.lock().unwrap().clone();
        if let Some(barrier) = barrier {
            barrier.wait();
        }
    }

    // A database whose recorded version is ahead of the latest known migration
    // predates the consolidated V1 baseline (ADR-015) — e.g. it still carries the
    // pre-consolidation V2..V22 ledger — or was written by a newer build. Either
    // way the baseline schema would be silently skipped, leaving the process on a
    // stale schema. Fail loudly instead of corrupting silently.
    let latest_version = latest_schema_version();
    if current_version > latest_version {
        return Err(SqliteError::InvalidData(format!(
            "database schema version {current_version} is ahead of the latest known migration \
             {latest_version}. This database predates the consolidated baseline (ADR-015) or was \
             written by a newer build. Recreate it from the current schema; in-place downgrade is \
             not supported."
        )));
    }

    // Every writable upgrade starts from an exact canonical version sequence;
    // fail before applying new migrations if MAX(version) hides a missing or
    // foreign row. A pre-V19 database may still carry the V13/V14 name
    // divergence that V19 exists to repair. Name validation therefore permits
    // exactly those two rows before V19, while every unrelated mismatch still
    // fails before any new migration is applied.
    let applied = read_applied_migration_ledger(conn, current_version)?;
    validate_applied_migration_versions(&applied, current_version)?;
    validate_applied_migration_names(&applied, current_version, current_version < 19)?;

    let mut applied_version = current_version;
    // Floor advanced when a sibling's work is observed under the write lock,
    // so a losing process skips the remaining already-applied migrations
    // without opening a transaction for each.
    let mut skip_through = current_version;

    for migration in MIGRATIONS {
        if migration.version <= skip_through {
            applied_version = applied_version.max(migration.version);
            continue;
        }

        // IMMEDIATE: take the write lock up front so concurrent boots serialize
        // here instead of failing mid-migration when a DEFERRED transaction
        // upgrades to a write.
        #[cfg(test)]
        let instrumented_first_begin = test_sync::PARTICIPATE.with(|p| p.get())
            && !test_sync::FIRST_BEGIN_DONE.with(|f| f.get());
        #[cfg(test)]
        if instrumented_first_begin {
            test_sync::FIRST_BEGIN_DONE.with(|f| f.set(true));
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        // Re-check under the write lock: a sibling process may have applied
        // this migration (and possibly later ones) while we waited. Running
        // its DDL again would fail; fast-forward past everything it applied.
        let sibling_version: u32 = tx
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;
        #[cfg(test)]
        if instrumented_first_begin {
            use std::sync::atomic::Ordering::SeqCst;
            if sibling_version == 0 {
                // Winner: hold the write lock until SQLite has reported a
                // busy acquisition to the loser (its busy handler fired) —
                // proof the loser's BEGIN is actually blocked on this held
                // lock, not merely intended. Bounded so a regression fails
                // the assertion instead of hanging the test.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !test_sync::BUSY_OBSERVED.load(SeqCst) && std::time::Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
            } else {
                // Loser: our first BEGIN just returned. Record whether the
                // winner had already committed — true means we blocked across
                // its held lock.
                test_sync::LOSER_SAW_WINNER_COMMIT
                    .store(test_sync::WINNER_COMMITTED.load(SeqCst), SeqCst);
            }
        }

        // The ahead-of-latest guard above ran on a pre-lock read; a newer
        // build may have committed a version past ours while we waited for
        // the write lock. Accepting it (clamped) would return Ok on a schema
        // this binary does not understand — reject it the same way.
        if sibling_version > latest_version {
            return Err(SqliteError::InvalidData(format!(
                "database schema version {sibling_version} is ahead of the latest known \
                 migration {latest_version} (committed by a concurrent process while this \
                 one waited for the migration write lock). This build cannot run against \
                 the newer schema; upgrade the binary or recreate the database."
            )));
        }

        if sibling_version >= migration.version {
            #[cfg(test)]
            test_sync::LOCKED_FAST_FORWARDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            skip_through = sibling_version.min(latest_version);
            applied_version = applied_version.max(migration.version);
            continue;
        }

        if migration.version == ATTACHMENT_CUTOVER_VERSION {
            let status = attachment_cutover_status(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;
            let legacy_refs: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM entities WHERE content_ref IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;

            // V21 belongs to the boot coordinator. Ordinary backend open may
            // finish the degenerate zero-ref case atomically, but it must not
            // expose a dual-source interval or eagerly stage a legacy DB.
            if status == AttachmentCutoverStatus::Incomplete || legacy_refs != 0 {
                drop(tx);
                break;
            }
            if status != AttachmentCutoverStatus::Pending {
                return Err(SqliteError::Migration {
                    version: migration.version,
                    error: format!("unexpected attachment cutover state {status:?}"),
                });
            }

            let now = chrono::Utc::now().timestamp_micros();
            stage_attachment_cutover_on_connection(&tx, now).map_err(|e| {
                SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                }
            })?;
            finalize_attachment_cutover_on_connection(&tx, now).map_err(|e| {
                SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                }
            })?;
        } else {
            tx.execute_batch(migration.up)
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
        }

        // V19's repair contract includes normalizing the two known-divergent
        // recorded names. `_schema_migrations` is created and owned by this
        // runner (not by any migration file), so the normalization lives
        // here, in the same transaction that applies V19's SQL. Exact,
        // closed set — versions 13 and 14 only; any other (version, name)
        // mismatch still fails startup via validate_applied_migration_ledger.
        if migration.version == 19 {
            tx.execute_batch(
                "UPDATE _schema_migrations SET name = 'list_cursor_sequences' WHERE version = 13;\n\
                 UPDATE _schema_migrations SET name = 'graph_edges_id_unique' WHERE version = 14;",
            )
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;
        }

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(version) DO NOTHING",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        #[cfg(test)]
        if instrumented_first_begin {
            test_sync::WINNER_COMMITTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        applied_version = migration.version;
    }

    // Validate again after the loop: our own commits and any under-lock
    // sibling fast-forward must both leave the exact canonical ledger, not
    // merely advance its maximum version.
    validate_applied_migration_ledger(conn, applied_version)?;

    Ok(applied_version)
}

#[derive(Debug)]
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
/// Opens the database at `db` (defaults to `~/.khive/khive.db`) and
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
            .join(".khive/khive.db")
    });
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    query_embedding_models_conn(&conn, engine_filter)
}

/// Query `_embedding_models` from an existing connection (testable without a file).
///
/// Returns an empty vec if the table does not exist.
pub(crate) fn query_embedding_models_conn(
    conn: &Connection,
    engine_filter: Option<&str>,
) -> Result<Vec<EmbeddingModelRegistryRecord>, SqliteError> {
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
        let dim_raw: i64 = row.get(3)?;
        let dimensions = u32::try_from(dim_raw).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::other(format!(
                    "_embedding_models.dim value {dim_raw} is outside the valid u32 range [0, {}]",
                    u32::MAX,
                ))),
            )
        })?;
        Ok(EmbeddingModelRegistryRecord {
            engine_name: row.get(0)?,
            model_id: row.get(1)?,
            key_version: row.get(2)?,
            dimensions,
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
