//! Background workspace mirror — folds `.khive/` filesystem content into the
//! graph substrate as `document` entities (ADR-087).
//!
//! Module shape mirrors `khive-pack-session/src/mirror/`: the operational
//! pattern (a `warm()`-spawned poller, a pack-owned cursor table, nonzero-
//! clamped poll interval, and unconditional secret masking) is reused from
//! ADR-080 §6 verbatim. Only the write target diverges — content lands in
//! real `document` entities via the same internal path (`KhiveRuntime::
//! create_entity`) an agent's `create` call would use, per ADR-086's shape,
//! instead of a pack-private auxiliary table. See ADR-087's "critical
//! divergence" section for why the storage target must not also be copied.

pub mod glob;
pub mod ingest;
pub mod service;

pub use ingest::{mirror_file, MirrorOutcome};
pub use service::{run_mirror_service, MirrorConfig};

/// Idempotent DDL for the mirror's own cursor-tracking table.
///
/// Pack-owned bookkeeping, deliberately kept OUTSIDE the entity/note/edge
/// substrate (ADR-087 Decision item 1: "a cursor is bookkeeping, not
/// content") — the one piece of this mirror that stays auxiliary, exactly
/// as `khive-pack-session`'s own `session_mirror_cursor` table does.
/// Applied via `KgPack::schema_plan()`, the same declarative pack-schema
/// mechanism (ADR-028) the session pack uses for its own cursor table —
/// mirroring ITS storage mechanism exactly, since the session mirror's
/// cursor never lived in the central `khive-db` migrations either.
pub const WORKSPACE_MIRROR_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS workspace_mirror_cursor (\
        path TEXT PRIMARY KEY, \
        last_mtime INTEGER NOT NULL, \
        last_hash TEXT NOT NULL, \
        last_synced_at INTEGER NOT NULL\
     )",
    "CREATE INDEX IF NOT EXISTS idx_workspace_mirror_cursor_synced \
        ON workspace_mirror_cursor(last_synced_at)",
];
