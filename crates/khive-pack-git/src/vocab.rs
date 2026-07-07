//! Git pack vocabulary: note kind specs and the pack-auxiliary cursor schema.
//!
//! No `HANDLERS` and no `EDGE_RULES` are declared here (unlike gtd): this pack
//! introduces zero new verbs and relies exclusively on the base `annotates`
//! contract (note -> any substrate) for provenance edges — no endpoint
//! extension is needed. See `crates/khive-pack-git/src/pack.rs`.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};

/// Lifecycle declaration shared by `issue` and `pull_request` — both track an
/// open/closed state with the same posture as ADR-088's `finding` precedent:
/// declared for introspection, not yet enforced by the runtime (Phase 1).
const GIT_LIFECYCLE: NoteLifecycleSpec = NoteLifecycleSpec {
    field: "kind_status",
    initial: "open",
    terminal: &["closed"],
    transitions: &[("open", "closed"), ("closed", "open")],
};

/// Note kind specs for the two lifecycle-bearing kinds this pack contributes.
///
/// `commit` deliberately has no entry: commits are immutable and carry no
/// lifecycle field.
pub(crate) static GIT_NOTE_KIND_SPECS: [NoteKindSpec; 2] = [
    NoteKindSpec {
        kind: "issue",
        aliases: &[],
        lifecycle: GIT_LIFECYCLE,
    },
    NoteKindSpec {
        kind: "pull_request",
        aliases: &[],
        lifecycle: GIT_LIFECYCLE,
    },
];

/// Pack-auxiliary schema: the git-ingest cursor table (ADR-088 §5, ADR-087
/// operational pattern reused).
///
/// Shape is intentionally generic across git record kinds within a project —
/// `kind` distinguishes `commits` / `issues` / `prs` cursors so a follow-up
/// pack (e.g. a code-review pack) can reuse this exact table for its own
/// cursor rows without a schema change, keyed by its own `project_id`/`kind`
/// pair. Idempotent (`CREATE TABLE IF NOT EXISTS`), applied once at pack
/// registration time; not part of the core versioned migration chain.
pub(crate) static GIT_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS git_mirror_cursor (\
        project_id   TEXT NOT NULL,\
        kind         TEXT NOT NULL,\
        cursor_value TEXT,\
        updated_at   INTEGER NOT NULL,\
        PRIMARY KEY (project_id, kind)\
    )",
    "CREATE INDEX IF NOT EXISTS idx_git_mirror_cursor_updated \
        ON git_mirror_cursor(updated_at DESC)",
];
