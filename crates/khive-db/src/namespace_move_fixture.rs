//! A store that reproduces the namespace split, built row by row.
//!
//! The mover's contract is a refusal: when a record cannot be carried to its
//! target because a row already there occupies the same unique key, the whole
//! move is refused and the colliding rows are named, never resolved by picking
//! a winner. A fixture for that has to CONTAIN collisions, so [`FixtureSpec`]
//! takes them as a parameter rather than offering a clean store beside a second
//! helper: the clean arm and the colliding arm differ in exactly one input, or a
//! green clean run says nothing about the refusal.
//!
//! # What a plain INSERT here does and does not produce
//!
//! Rows are written with statements rather than through a pack's writer, and
//! that is a decision about fidelity, not a shortcut. The three shapes differ:
//!
//! - The list-cursor sequence rows ARE produced. `assign_note_list_seq`,
//!   `assign_entity_list_seq` and `assign_graph_edge_list_seq` are `AFTER
//!   INSERT` triggers (`sql/019-list-cursor-backfill-repair.sql:56-76`), so a
//!   statement-level insert leaves the same ledger row a pack write would.
//! - `fts_notes` and `fts_entities` rows are NOT produced. No trigger in `sql/`
//!   names either table; their contents are written from Rust
//!   (`stores::text`), which is why the move writes them itself. So the builder
//!   writes them the same way that store does: the FTS insert, then the
//!   rowid-map upsert off `last_insert_rowid()`, on the same connection with
//!   nothing written in between, because that function is connection-scoped and
//!   reports whichever INSERT ran last.
//! - `fts_knowledge` and `fts_sections` rows ARE produced, by trigger, from the
//!   base row (`sql/026-knowledge-fts-repair.sql:36-56`,
//!   `sql/002-narrow-fts-sections-update-trigger.sql:9-15`). Both are fts5 over
//!   external content, so writing them directly is a no-op that reports
//!   success — which is why the builder does not write them, and why an arm
//!   exists to hold that line.
//!
//! # Ids
//!
//! Every id is a literal. A fixture whose ids move between runs cannot assert
//! WHICH rows a refusal named, and naming them is the contract. They are UUIDs
//! rather than readable words because the text store parses `subject_id` as a
//! `Uuid`, so a word would make the fixture unusable from any arm that reaches
//! store code.

use rusqlite::{params, Connection};

/// The note that moves to the kg pack, and the subject of the note-key
/// collision, the stale-index collision and the ordinary-fts5 arm.
pub const NOTE_OBSERVATION: &str = "11111111-1111-4111-8111-000000000001";
/// A second note under the same source namespace whose route is a DIFFERENT
/// target pack. One source namespace fanning out to several targets is the
/// ordinary case, not an edge case, and a fixture with one target cannot tell a
/// per-target report from a global one.
pub const NOTE_TASK: &str = "11111111-1111-4111-8111-000000000002";
/// Soft-deleted, and carrying the same `(kind, key)` as a live row under the
/// target. `idx_notes_namespace_kind_key` is partial on `deleted_at IS NULL`,
/// so this row must NOT be reported as a collision.
pub const NOTE_DELETED: &str = "11111111-1111-4111-8111-000000000003";
/// Already under the kg target, holding the `(kind, key)` [`NOTE_OBSERVATION`]
/// would need.
pub const NOTE_TARGET_HOLDER: &str = "11111111-1111-4111-8111-000000000004";

pub const ENTITY: &str = "22222222-2222-4222-8222-000000000001";

/// An edge whose endpoints route to different packs.
pub const EDGE: &str = "33333333-3333-4333-8333-000000000001";
/// Already under the kg target, holding the
/// `(namespace, source_id, target_id, relation)` triple [`EDGE`] would need.
pub const EDGE_TRIPLE_HOLDER: &str = "33333333-3333-4333-8333-000000000002";

pub const ATOM: &str = "44444444-4444-4444-8444-000000000001";
/// Already under the knowledge target, holding [`ATOM`]'s slug.
pub const ATOM_SLUG_HOLDER: &str = "44444444-4444-4444-8444-000000000002";
/// A child of [`ATOM`]: it carries its own `namespace` column and must move
/// with its parent or the pair straddles two namespaces.
pub const SECTION: &str = "55555555-5555-4555-8555-000000000001";

/// The rowid the planted stale index row points at. Deliberately a value no
/// `fts_notes` row has, since the row it belonged to is gone.
const STALE_FTS_ROWID: i64 = 90_001;

const T: i64 = 1_700_000_000;

/// The namespaces a fixture spans, and which collisions to plant.
///
/// The three collision flags are separate because the arms that consume them
/// have to be separable: an arm proving the stale-index shadow refuses a move
/// must run on a store where the base tables permit it, or it cannot say which
/// of the two refusals it observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureSpec {
    /// The bare tenant namespace an unbound session wrote to.
    pub source: String,
    /// The pack-bound namespaces a bound session writes to.
    pub kg: String,
    pub gtd: String,
    pub knowledge: String,
    /// Plant a note under the target holding the `(kind, key)` the move needs,
    /// an atom holding the slug, and an edge holding the triple.
    pub collisions: bool,
    /// Plant an `fts_notes_rowids` row under the target whose base record does
    /// not exist, carrying the subject id of a note the move would carry.
    ///
    /// This is the collision neither hand enumeration predicted. The map is
    /// keyed `(namespace, subject_id)`, nothing in `notes` shows a conflict,
    /// and the move is blocked by a shadow. The state is documented as
    /// reachable in the store itself: `stores::text::delete_document_statement`
    /// describes a crash between an FTS row's removal and its map row's
    /// removal, and `sql/024-fts-rowid-map.sql` keeps such rows on purpose
    /// rather than reconciling them. It is planted directly here because that
    /// crash is what produces it — there is no sequence of successful store
    /// calls that leaves one behind.
    pub stale_fts: bool,
}

impl FixtureSpec {
    /// The whole fixture: every collision planted.
    pub fn new(tenant: &str) -> Self {
        Self {
            source: tenant.to_string(),
            kg: format!("{tenant}.kg"),
            gtd: format!("{tenant}.gtd"),
            knowledge: format!("{tenant}.knowledge"),
            collisions: true,
            stale_fts: true,
        }
    }

    /// A store the move should be able to carry in full: the same rows, no
    /// occupied keys under any target.
    pub fn movable(tenant: &str) -> Self {
        Self {
            collisions: false,
            stale_fts: false,
            ..Self::new(tenant)
        }
    }
}

impl Default for FixtureSpec {
    fn default() -> Self {
        Self::new("tenant:tnt_fixture")
    }
}

/// Which optional rows a given [`build`] actually planted, so an arm reads the
/// store it was handed rather than re-deriving it from the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub spec: FixtureSpec,
    /// The note under the target occupying [`NOTE_OBSERVATION`]'s key.
    pub colliding_note: Option<&'static str>,
    pub colliding_atom: Option<&'static str>,
    pub colliding_edge_triple: Option<&'static str>,
    /// The subject id whose index row survives its base record.
    pub stale_fts_subject: Option<&'static str>,
}

/// Write one note, and the two index rows the note store would have written.
fn note(
    conn: &Connection,
    id: &str,
    ns: &str,
    kind: &str,
    key: Option<&str>,
    content: &str,
    deleted: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, key, content, created_at, updated_at, deleted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![id, ns, kind, key, content, T, deleted.then_some(T)],
    )?;
    index_row(conn, "fts_notes", id, ns, kind, content)
}

/// The FTS insert and its rowid-map upsert, in the order and adjacency
/// `stores::text` requires: `last_insert_rowid()` is connection-scoped, so
/// anything written between the two statements would map the key to the wrong
/// row.
fn index_row(
    conn: &Connection,
    table: &str,
    id: &str,
    ns: &str,
    kind: &str,
    body: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {table} \
             (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
             VALUES (?1, ?2, '', ?3, '[]', ?4, NULL, ?5, ?2)"
        ),
        params![id, kind, body, ns, T],
    )?;
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO {table}_rowids (namespace, subject_id, rowid) \
             VALUES (?1, ?2, last_insert_rowid())"
        ),
        params![ns, id],
    )?;
    Ok(())
}

/// Write the fixture into an already-migrated store.
pub fn build(conn: &Connection, spec: &FixtureSpec) -> rusqlite::Result<Fixture> {
    // The ordinary move, a second target pack out of the same source namespace,
    // and a soft-deleted row. The soft delete is load-bearing rather than
    // decorative: it is the half of `idx_notes_namespace_kind_key` a collision
    // check gets wrong in the refusing direction if it ignores the predicate.
    note(
        conn,
        NOTE_OBSERVATION,
        &spec.source,
        "observation",
        Some("shared-key"),
        "the ordinary move",
        false,
    )?;
    note(
        conn,
        NOTE_TASK,
        &spec.source,
        "task",
        None,
        "a second target pack",
        false,
    )?;
    note(
        conn,
        NOTE_DELETED,
        &spec.source,
        "observation",
        Some("shared-key"),
        "soft deleted",
        true,
    )?;

    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![ENTITY, &spec.source, "concept", "fixture concept", T],
    )?;
    index_row(
        conn,
        "fts_entities",
        ENTITY,
        &spec.source,
        "concept",
        "fixture concept",
    )?;

    conn.execute(
        "INSERT INTO graph_edges (id, namespace, source_id, target_id, relation, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![EDGE, &spec.source, ENTITY, NOTE_OBSERVATION, "annotates", T],
    )?;

    // The atom and a child section, which carries its own namespace column and
    // has to move with its parent. Both index themselves by trigger.
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![ATOM, &spec.source, "shared-slug", "fixture atom", T],
    )?;
    conn.execute(
        "INSERT INTO knowledge_sections \
         (id, atom_id, namespace, section_type, heading, content, content_hash, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![SECTION, ATOM, &spec.source, "body", "a heading", "section body", "hash-1", T],
    )?;

    let mut fixture = Fixture {
        spec: spec.clone(),
        colliding_note: None,
        colliding_atom: None,
        colliding_edge_triple: None,
        stale_fts_subject: None,
    };

    if spec.collisions {
        note(
            conn,
            NOTE_TARGET_HOLDER,
            &spec.kg,
            "observation",
            Some("shared-key"),
            "already here",
            false,
        )?;
        fixture.colliding_note = Some(NOTE_TARGET_HOLDER);

        conn.execute(
            "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                ATOM_SLUG_HOLDER,
                &spec.knowledge,
                "shared-slug",
                "already here",
                T
            ],
        )?;
        fixture.colliding_atom = Some(ATOM_SLUG_HOLDER);

        conn.execute(
            "INSERT INTO graph_edges (id, namespace, source_id, target_id, relation, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![EDGE_TRIPLE_HOLDER, &spec.kg, ENTITY, NOTE_OBSERVATION, "annotates", T],
        )?;
        fixture.colliding_edge_triple = Some(EDGE_TRIPLE_HOLDER);
    }

    if spec.stale_fts {
        conn.execute(
            "INSERT INTO fts_notes_rowids (namespace, subject_id, rowid) VALUES (?1, ?2, ?3)",
            params![&spec.kg, NOTE_OBSERVATION, STALE_FTS_ROWID],
        )?;
        fixture.stale_fts_subject = Some(NOTE_OBSERVATION);
    }

    Ok(fixture)
}

/// The route map this fixture's rows require, as `(route key, target)` pairs.
///
/// Every subject class present in the source is routed, because an unrouted
/// class with rows refuses the whole move while a routed class with no rows
/// succeeds reporting zero. So this list is not a convenience: it has to track
/// [`build`] exactly, and a row added there without a route here turns every
/// arm red with `UnroutedClass` rather than with whatever the arm was about.
///
/// Returned as strings rather than as the mover's own types so the fixture
/// stays buildable in a store that has no mover, which is the state this
/// module was first written in.
pub fn routes(spec: &FixtureSpec) -> Vec<(&'static str, String)> {
    vec![
        ("note:observation", spec.kg.clone()),
        ("note:task", spec.gtd.clone()),
        ("entity:concept", spec.kg.clone()),
        ("edge", spec.kg.clone()),
        ("atom", spec.knowledge.clone()),
    ]
}

/// Create one runtime vector table and put a row in it under `namespace`.
///
/// Separate from [`build`] because the table exists in no declaration — the
/// model key is part of its name, so no static enumeration of the schema can
/// see it, which is the half of the census a source-parsing instrument is blind
/// to. The DDL matches the one the vector store creates
/// (`stores::vectors`); `dims` is small because nothing here reads the vector.
pub fn add_vector_row(
    conn: &Connection,
    model_key: &str,
    namespace: &str,
    subject_id: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{model_key} USING vec0(\
         subject_id TEXT PRIMARY KEY, \
         namespace TEXT NOT NULL, \
         kind TEXT NOT NULL, \
         field TEXT NOT NULL, \
         embedding_model TEXT NOT NULL, \
         embedding float[4] distance_metric=cosine)"
    ))?;
    // The JSON text form vec0 accepts, matching how the move's own vector arm
    // seeds one. A byte-packed f32 blob works too, and reads like a different
    // thing being tested.
    conn.execute(
        &format!(
            "INSERT INTO vec_{model_key} \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES (?1, ?2, ?3, ?4, ?5, '[0.1, 0.2, 0.3, 0.4]')"
        ),
        params![subject_id, namespace, "note", "content", model_key],
    )?;
    Ok(())
}
