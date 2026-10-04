//! Moving records between namespaces (ADR-189).
//!
//! Namespace is attribution-only and an open string, so moving a record between
//! namespaces is sound in principle. It is not a `UPDATE ... SET namespace`
//! sweep, for reasons that are properties of this schema rather than matters of
//! taste: six of the affected tables are fts5 virtual tables that accept no
//! `UPDATE` of an indexed column, the vector tables are created at runtime and
//! appear in no static list, and the ANN bookkeeping has ordering semantics an
//! `UPDATE` violates silently.
//!
//! Three scoping facts decide the shape of everything below.
//!
//! **One backend.** A pack can be assigned its own backend, and then its records
//! live in a different SQLite file; the live configuration on a development
//! machine puts comm's notes and the knowledge atoms in two files beside the
//! main one. SQLite has no transaction across unattached databases, so this
//! operates on the connection it is given and a store with three backends is
//! the same route map applied three times. That composes because a class routed
//! with no rows here succeeds reporting zero. Atomicity does not compose, and
//! this module does not pretend otherwise.
//!
//! **Re-runnable, so a partial application is a resume point.** A backend whose
//! move did not run holds exactly the state that existed before anyone asked:
//! records under the source namespace. That is not a new failure mode, it is the
//! one the move was called to fix, still present for the unmoved subset. Because
//! a routed class with no rows succeeds reporting zero, a second run over an
//! already-moved backend is a no-op and a second run over the failed one is a
//! first run. So what a multi-backend caller owes is not all-or-nothing across
//! files, which SQLite cannot give it, but per-backend atomicity, a per-backend
//! report so a partial outcome is known rather than silent, and the willingness
//! to run the same request again.
//!
//! **No transaction of its own.** `WriterTaskHandle::send` hands its closure a
//! connection already inside the `BEGIN IMMEDIATE` it opened and owns the commit
//! or rollback, and a nested bare `BEGIN IMMEDIATE` is an error. So the entry
//! point here is DML-only, and the enumerating `SELECT`s run inside the caller's
//! transaction with the writes they feed — the same TOCTOU reason
//! `Fts5TextSearch::rename_namespace` records.
//!
//! **Refuse rather than resolve.** A collision means two rows the caller wrote
//! claim one identity in the target namespace. There is no conflict policy: any
//! `ON CONFLICT` form picks a winner over a caller's data while satisfying a
//! counts-in-equals-counts-out assertion, which is the worst available pairing —
//! the destructive outcome and the reassuring receipt arrive together.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension};

use crate::namespace_census::{self, NamespaceCensus, NamespaceConstraint};

#[path = "namespace_move_knowledge.rs"]
mod knowledge_move;
#[path = "namespace_move_pack_tables.rs"]
mod pack_tables;
#[path = "namespace_move_routes.rs"]
mod subject_routes;
use knowledge_move::{move_knowledge_atoms, ordinary_atom_count};
use pack_tables::{move_task_audit, settle_pack_tables};
use subject_routes::routed_subjects;

/// A routable subject class: a record that exists in its own right.
///
/// Notes and entities qualify by kind; edges optionally qualify by relation.
/// Nothing in the schema
/// stops a note kind and an entity kind sharing a spelling, so an unqualified
/// key would route both on a store that has them, and a refusal naming
/// `note:observation` is one a caller can act on where `observation` is not.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubjectClass {
    Note(String),
    Entity(String),
    Edge,
    EdgeRelation(String),
    Atom,
    Domain,
}

impl SubjectClass {
    /// Parse a route key. The error names what was given, because a typo here
    /// is the most likely caller mistake and the least likely to be obvious.
    pub fn parse(key: &str) -> Result<Self, MoveError> {
        match key {
            "edge" => return Ok(Self::Edge),
            "atom" => return Ok(Self::Atom),
            "domain" => return Ok(Self::Domain),
            _ => {}
        }
        match key.split_once(':') {
            Some(("note", kind)) if !kind.is_empty() => Ok(Self::Note(kind.to_string())),
            Some(("entity", kind)) if !kind.is_empty() => Ok(Self::Entity(kind.to_string())),
            Some(("edge", relation))
                if khive_types::EdgeRelation::VALID_NAMES.contains(&relation) =>
            {
                Ok(Self::EdgeRelation(relation.to_string()))
            }
            _ => Err(MoveError::UnknownSubjectClass {
                key: key.to_string(),
            }),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Self::Note(kind) => format!("note:{kind}"),
            Self::Entity(kind) => format!("entity:{kind}"),
            Self::Edge => "edge".to_string(),
            Self::EdgeRelation(relation) => format!("edge:{relation}"),
            Self::Atom => "atom".to_string(),
            Self::Domain => "domain".to_string(),
        }
    }
}

/// One `(subject class, target namespace)` pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRoute {
    pub class: SubjectClass,
    pub target: String,
}

/// A whole move, validated before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    pub source: String,
    pub routes: Vec<MoveRoute>,
    /// The instant, in microseconds since the epoch, at which grant expiry is
    /// judged when authorization rows stay behind. `None` reads the wall clock
    /// once, at the call. Set it with [`MoveRequest::at`] so a test, or a caller
    /// replaying a move, controls it.
    pub now_micros: Option<i64>,
}

impl MoveRequest {
    /// The route map as data, validated whole before the first write. A list can
    /// be checked, logged, replayed and tested against a fixture; a callback
    /// can do none of those.
    pub fn new(source: impl Into<String>, routes: Vec<MoveRoute>) -> Self {
        Self {
            source: source.into(),
            routes,
            now_micros: None,
        }
    }

    /// Judge grant expiry at `now_micros` instead of at the wall clock.
    pub fn at(mut self, now_micros: i64) -> Self {
        self.now_micros = Some(now_micros);
        self
    }

    fn route_for(&self, class: &SubjectClass) -> Option<&MoveRoute> {
        self.routes
            .iter()
            .find(|route| &route.class == class)
            .or_else(|| match class {
                SubjectClass::EdgeRelation(_) => self
                    .routes
                    .iter()
                    .find(|route| route.class == SubjectClass::Edge),
                _ => None,
            })
    }

    /// Every route names the same target, and every class present in the source
    /// is routed. Only then can a per-namespace aggregate with no subject be
    /// carried anywhere.
    fn single_target(&self) -> Option<&str> {
        let mut targets = self.routes.iter().map(|r| r.target.as_str());
        let first = targets.next()?;
        targets.all(|t| t == first).then_some(first)
    }
}

/// What a move did, per route and per table.
///
/// `left_behind` is not an error column. A per-namespace aggregate with no
/// subject cannot be split across a partitioning move; separately identified
/// operational records have no subject route at all. They stay, and the caller
/// is told rather than left to discover them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveCounts {
    /// Subjects moved, keyed by the rendered route key. A routed class with no
    /// rows appears here with zero — that is what makes the map verifiable by
    /// its caller, and what makes the same map re-runnable against each backend
    /// of a split store.
    pub subjects: BTreeMap<String, u64>,
    /// Rows written, keyed by table. Derived rows appear here and in no route.
    pub rows: BTreeMap<String, u64>,
    /// Rows left where they were, keyed by table, with the reason in the docs
    /// above rather than in the data.
    pub left_behind: BTreeMap<String, u64>,
    /// Entries appended to `ann_write_log` for the source and target namespaces.
    pub ann_log_appended: u64,
    /// Tool policy rows left in the source that are not soft-deleted. Authorization
    /// state is never carried, so these are the rows an operator still has to
    /// decide about; `left_behind` counts every row that stayed.
    pub live_policies_left_behind: u64,
    /// Tool grant rows left in the source that are in force at the instant the
    /// move judged them (see [`MoveRequest::now_micros`]): granted, not
    /// invalidated by a registration, and not expired.
    pub grants_in_force_left_behind: u64,
}

/// A note that cannot move, and its position in the stream that pins it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamMember {
    pub note_id: String,
    pub stream: String,
    pub seq: i64,
}

/// A row that would claim an identity already taken in the target namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    pub table: String,
    pub constraint: String,
    pub target: String,
    /// The key values that clash, rendered in the constraint's own column order.
    ///
    /// This identifies both rows and needs no second field: the row already in
    /// the target holds this key there, and the row that would have moved holds
    /// the same key in the source namespace.
    pub key: String,
}

/// Why a move refused, or how it failed.
#[derive(Debug)]
pub enum MoveError {
    /// A route key that names no subject class.
    UnknownSubjectClass {
        key: String,
    },
    /// The same class routed twice. Ambiguous rather than redundant: the two
    /// targets may differ, and picking one would be a guess.
    DuplicateRoute {
        class: String,
    },
    /// A route whose target is the source. A no-op written as an instruction is
    /// more likely a mistake than an intent.
    TargetIsSource {
        class: String,
    },
    /// A namespace-bearing table holding rows here that this code has no rule
    /// for.
    ///
    /// The census finds tables; only a reader of the code can say what a move
    /// does with one. So a table arriving in a migration after this was written
    /// refuses the move by name, rather than letting the subjects around it move
    /// and leaving the new table pointing at a namespace nothing else is in.
    UnknownTable {
        table: String,
        rows: u64,
    },
    /// A subject class with rows in the source namespace and no route.
    ///
    /// Distinct from a routed class with no rows, which succeeds reporting zero.
    /// A host that binds a pack writing nothing needs "routed, nothing there" to
    /// be distinguishable from "you forgot this one".
    UnroutedClass {
        class: String,
        rows: u64,
    },
    /// Notes the stream schema pins to their namespace.
    ///
    /// Four triggers make this absolute: an `UPDATE` of a member note naming
    /// `namespace` aborts, the delete aborts, and both writes to the ledger row
    /// abort. So the refusal comes from a read taken before any write, and names
    /// the notes, rather than from a trigger's abort string from somewhere in
    /// the middle of the transaction.
    StreamMembers {
        notes: Vec<StreamMember>,
    },
    /// A partitioned vector has no unique destination among its source subjects.
    /// Single-target moves retain their existing whole-namespace behavior.
    UnroutableVector {
        subject_id: String,
        table: String,
        destinations: u64,
    },
    /// Rows that would collide in a target namespace.
    Collisions {
        collisions: Vec<Collision>,
    },
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for MoveError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSubjectClass { key } => write!(
                f,
                "no subject class named {key:?}; expected note:<kind>, entity:<kind>, edge:<relation>, edge, atom or domain"
            ),
            Self::DuplicateRoute { class } => {
                write!(f, "{class} is routed more than once")
            }
            Self::TargetIsSource { class } => {
                write!(f, "{class} is routed to the namespace it is already in")
            }
            Self::UnknownTable { table, rows } => write!(
                f,
                "{table} holds {rows} row(s) in the source namespace and this build has no rule \
                 for it; a table added by a later migration refuses a move rather than being \
                 left behind by one"
            ),
            Self::UnroutedClass { class, rows } => write!(
                f,
                "{class} has {rows} row(s) in the source namespace and no route; \
                 a class routed with zero rows succeeds reporting zero, an unrouted one refuses"
            ),
            Self::StreamMembers { notes } => {
                write!(f, "{} note(s) belong to a stream and cannot change namespace: ", notes.len())?;
                for (i, member) in notes.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} ({} seq {})", member.note_id, member.stream, member.seq)?;
                }
                Ok(())
            }
            Self::UnroutableVector { subject_id, table, destinations } => write!(
                f,
                "unroutable_vector: {table:?} subject {subject_id:?} has {destinations} source-subject destinations; a partitioned move requires exactly one"
            ),
            Self::Collisions { collisions } => {
                write!(f, "{} collision(s): ", collisions.len())?;
                for (i, collision) in collisions.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(
                        f,
                        "{}.{} in {} already holds {}",
                        collision.table, collision.constraint, collision.target, collision.key
                    )?;
                }
                Ok(())
            }
            Self::Sqlite(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for MoveError {}

/// Tables holding rows keyed by namespace with no subject to be carried by.
///
/// `brain_profile_snapshots` is `(profile_id, namespace)` and `brain_event_log`
/// likewise: one row per profile per namespace, describing an aggregate. A move
/// routing five classes to five namespaces has no target to carry a single
/// profile snapshot to, and splitting it would invent numbers. They move only
/// when the move is total and single-target.
const NAMESPACE_SCOPED_TABLES: &[&str] = &[
    "brain_profile_snapshots",
    "brain_event_log",
    "proposals_open",
];

/// Tables keyed by a subject the caller routes, which therefore follow it.
///
/// Both are `(namespace, target_id, …)` shapes in the brain pack, and `target_id`
/// names a note, an entity or an atom. That is why the follow query below is a
/// union over the three subject tables rather than a per-route join: the column
/// does not say which kind of subject it points at, and a wrong guess would
/// leave learned state under a namespace its subject has left.
const SUBJECT_KEYED_TABLES: &[&str] = &["brain_implicit_mass", "brain_serve_ledger"];

/// A memory visibility receipt belongs to its note. Its fences name the
/// receipt by `(namespace, note_id)`, so they must follow it in dependency order.
const MEMORY_VISIBILITY_RECEIPTS: &str = "memory_visibility_receipts";
const MEMORY_VISIBILITY_FENCES: &str = "memory_visibility_fences";

/// Sender envelopes have a logical-message/device/epoch identity and survive
/// deletion of their outbound note. The session mirror uses provider session
/// identities, not the IDs of caller-authored `session` notes. Neither has a
/// route in `MoveRequest`, so retain their source attribution and report it.
const LEAVE_BEHIND_TABLES: &[&str] = &["comm_sender_transport", "sessions", "session_messages"];

/// What a move does with one namespace-bearing table.
///
/// This is the half of the design that cannot be derived, because it is a
/// statement about meaning rather than about schema. The census answers which
/// tables carry the column; only a reader of the code can say whether a row in
/// one of them is a subject that moves, a projection that follows, an append-only
/// record of something that already happened, or an aggregate that cannot be
/// split. So the list is written down — and the guard is that a table the census
/// finds and this function does not name is a REFUSAL, not a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableDisposition {
    /// Rows the caller routes by subject class.
    Subject,
    /// A projection rebuilt from its subject, never routed on its own.
    ///
    /// Split two ways on purpose. `fts_knowledge` and `fts_sections` are
    /// maintained by triggers that fire on `UPDATE OF ... namespace`
    /// (`sql/026-knowledge-fts-repair.sql:49` and
    /// `sql/002-narrow-fts-sections-update-trigger.sql:10`), so writing the base
    /// row carries them. Both live declarations are column-scoped, and
    /// `fts_sections_au` reached that shape by being narrowed: `sql/schema.sql`
    /// declares it `AFTER UPDATE` unconditioned and V2 drops and recreates it
    /// over a named column list, to stop reindex-only updates from paying an
    /// fts5 delete-and-reinsert. So this disposition depends on `namespace`
    /// staying in that list, which a later narrowing could shorten without
    /// touching anything here. `fts_notes` and `fts_entities` have no triggers at all —
    /// their contents are written from Rust — so the move writes them itself, and
    /// because fts5 refuses an `UPDATE` of an indexed column that write is a
    /// delete followed by an insert.
    Derived { trigger_maintained: bool },
    /// The rowid map beside an fts5 table, keyed `(namespace, subject_id)`.
    DerivedRowidMap,
    /// Appended to, never rewritten: new entries land under the target namespace
    /// and the entries already there stay where they are, because they record
    /// writes that happened under the old name.
    Appended,
    /// A record of what happened. Rewriting it would make the audit trail
    /// describe a past that did not occur.
    History,
    /// Consumer bookkeeping whose ordering semantics an `UPDATE` violates.
    ConsumerWatermark,
    /// The schema itself refuses: `sql/029-note-streams.sql` installs four
    /// triggers that abort any namespace change to a member note or its ledger.
    RefusedBySchema,
    /// Keyed by `(profile_id, namespace)` with no subject, so a partitioning
    /// move has no target to carry it to. Moves only when the request is total
    /// and single-target; otherwise reported as left behind.
    NamespaceScopedAggregate,
    /// Keyed by a subject the caller routes, so it follows that subject.
    SubjectKeyed { subject_column: &'static str },
    /// Owns an identity outside the routed subject set. Keep its namespace and
    /// report the retained rows so a caller can handle them separately.
    LeaveBehind,
}

/// The disposition of a table, or `None` if this code has never seen it.
///
/// `None` is the whole point. A migration that adds a namespace-bearing table
/// after this was written lands in no branch here, and a move that finds rows in
/// it refuses by name rather than moving the subjects around it and leaving the
/// new table pointing at a namespace nothing else is in.
pub fn disposition(table: &namespace_census::NamespaceTable) -> Option<TableDisposition> {
    use TableDisposition::*;
    Some(match table.name.as_str() {
        "notes" | "entities" | "graph_edges" | "knowledge_atoms" | "knowledge_domains" => Subject,

        // Partitioned sections follow their source atom. A routed single-target
        // knowledge class carries every source section with its source vectors.
        "knowledge_sections" => SubjectKeyed {
            subject_column: "atom_id",
        },

        // An open proposal is keyed by `proposal_id` and references no subject,
        // so it belongs to the namespace rather than to anything inside it. In a
        // move routing several classes to several targets there is no namespace
        // for it to belong to afterwards, which is the same problem the two brain
        // aggregates have and takes the same answer.
        "proposals_open" => NamespaceScopedAggregate,

        "fts_knowledge" | "fts_sections" => Derived {
            trigger_maintained: true,
        },
        "fts_notes" | "fts_entities" => Derived {
            trigger_maintained: false,
        },
        "fts_notes_rowids" | "fts_entities_rowids" => DerivedRowidMap,
        // `move_vectors` clears these rows using the staged vec0 subjects.
        "vector_provenance" => Derived {
            trigger_maintained: false,
        },

        "ann_write_log" => Appended,
        "events" => History,
        "ann_consumer_watermark" | "ann_consumer_pending" => ConsumerWatermark,
        "note_streams" => RefusedBySchema,

        "brain_profile_snapshots" | "brain_event_log" => NamespaceScopedAggregate,
        "brain_implicit_mass" | "brain_serve_ledger" => SubjectKeyed {
            subject_column: "target_id",
        },
        "memory_visibility_receipts" => SubjectKeyed {
            subject_column: "note_id",
        },
        "memory_visibility_fences" => SubjectKeyed {
            subject_column: "note_id",
        },

        // The next eight are created by a pack, not by a migration, so a store
        // may not have them. Their rules live in `namespace_move_pack_tables.rs`.
        //
        // Readers of a task's audit rows key on `note_id` alone, so a transition
        // record belongs to its task note and follows it. `namespace` is nullable
        // here, and a NULL row is outside every namespace.
        "gtd_lifecycle_audit" => SubjectKeyed {
            subject_column: "note_id",
        },
        // One summary row per evaluation run, read per namespace, naming no
        // subject: the same problem the brain aggregates have.
        "knowledge_eval_runs" => NamespaceScopedAggregate,
        // Receipts of work done in the namespace, keyed to no routed subject, so
        // they follow the evaluation runs: a total single-target move carries
        // them and a partitioning move leaves them. An `exec_events` row belongs
        // to an `exec_runs` row of the same namespace and both take the same
        // branch, so the two tables always move or stay together.
        "exec_runs" | "exec_events" | "git_receipts" => NamespaceScopedAggregate,
        // Authorization state. A row carried into a target would grant, or deny,
        // there what the target never decided, so no move carries either table.
        // The rows stay in the source and the move reports them.
        "tool_policy" | "tool_grants" => LeaveBehind,
        // A cache of an index over the namespace it names, keyed by a bare
        // namespace or by `{namespace}::vamana::{model}`. The move deletes the
        // source's rows and writes nothing for the target.
        "retrieval_snapshots" => Derived {
            trigger_maintained: false,
        },
        _ if LEAVE_BEHIND_TABLES.contains(&table.name.as_str()) => LeaveBehind,

        // Created at runtime, one per embedding model, and in no source file, so
        // the live store is the only place they can be identified from.
        //
        // The NAME is not enough to identify one, and treating it as enough put a
        // hole straight through the refusal above: a migration adding an ordinary
        // namespace-bearing table called `vec_audit` would be classed here, handed
        // to `move_vectors`, and die on `no such column: embedding` in the middle
        // of the caller's transaction -- a bare SQLite error in place of the
        // refusal by name that every other unnamed table gets. A vector table is a
        // `CREATE VIRTUAL TABLE`, which an ordinary migration's table is not, so
        // the census's own reading of that is the second half of the test.
        _ if is_runtime_vector_table(table) => Derived {
            trigger_maintained: false,
        },

        _ => return None,
    })
}

/// A vector table created at runtime by an embedding model.
///
/// One predicate rather than two spellings of `starts_with("vec_")`: the
/// disposition and the loop that moves them have to agree, or a table one of
/// them admits reaches code the other never cleared.
fn is_runtime_vector_table(table: &namespace_census::NamespaceTable) -> bool {
    table.name.starts_with("vec_") && table.virtual_table
}

/// What the source namespace actually holds, read inside the caller's
/// transaction with the writes it feeds.
#[derive(Debug, Default)]
struct SourceInventory {
    /// `notes` and `entities` rows per `kind`, which is what a route key names.
    note_kinds: BTreeMap<String, u64>,
    entity_kinds: BTreeMap<String, u64>,
    edge_relations: BTreeMap<String, u64>,
    atoms: u64,
    domains: u64,
    /// Namespace-bearing tables holding rows here that [`disposition`] has no
    /// rule for. Non-empty means refuse.
    unknown: Vec<(String, u64)>,
}

fn count_in_namespace(conn: &Connection, table: &str, namespace: &str) -> rusqlite::Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE namespace = ?1",
        namespace_census::quote_ident(table)
    );
    conn.query_row(&sql, [namespace], |row| row.get::<_, i64>(0))
        .map(|n| n as u64)
}

fn kinds_in_namespace(
    conn: &Connection,
    table: &str,
    namespace: &str,
) -> rusqlite::Result<BTreeMap<String, u64>> {
    let sql = format!(
        "SELECT kind, COUNT(*) FROM {} WHERE namespace = ?1 GROUP BY kind",
        namespace_census::quote_ident(table)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([namespace], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (kind, count) = row?;
        out.insert(kind, count);
    }
    Ok(out)
}

fn edge_relations_in_namespace(
    conn: &Connection,
    namespace: &str,
) -> rusqlite::Result<BTreeMap<String, u64>> {
    conn.prepare(
        "SELECT relation, COUNT(*) FROM graph_edges WHERE namespace = ?1 GROUP BY relation",
    )?
    .query_map([namespace], |row| {
        Ok((row.get(0)?, row.get::<_, i64>(1)? as u64))
    })?
    .collect()
}

/// Soft-deleted rows are counted and moved with the rest, deliberately.
///
/// They are rows, they carry the namespace, and several of the partial unique
/// indexes exclude them, so they cannot collide. Leaving them behind would
/// strand a record's tombstone in a namespace its subject no longer occupies,
/// which is the state the delete path reads when it decides whether a later
/// write is a resurrection.
fn read_source(
    conn: &Connection,
    census: &NamespaceCensus,
    source: &str,
) -> Result<SourceInventory, MoveError> {
    let mut inventory = SourceInventory {
        note_kinds: kinds_in_namespace(conn, "notes", source)?,
        entity_kinds: kinds_in_namespace(conn, "entities", source)?,
        edge_relations: edge_relations_in_namespace(conn, source)?,
        atoms: ordinary_atom_count(conn, source)?,
        domains: count_in_namespace(conn, "knowledge_domains", source)?,
        unknown: Vec::new(),
    };

    for table in &census.tables {
        if disposition(table).is_some() {
            continue;
        }
        let count = count_in_namespace(conn, &table.name, source)?;
        if count > 0 {
            inventory.unknown.push((table.name.clone(), count));
        }
    }
    Ok(inventory)
}

/// Notes the stream schema pins where they are.
///
/// Read before any write, so the refusal names the notes rather than arriving as
/// a trigger's abort string from the middle of the transaction. Four triggers in
/// `sql/029-note-streams.sql` make it absolute: a namespace-naming `UPDATE` of a
/// member note aborts, the delete aborts, and both writes to the ledger abort.
fn stream_members(conn: &Connection, source: &str) -> rusqlite::Result<Vec<StreamMember>> {
    let mut stmt = conn.prepare(
        "SELECT note_id, stream, seq FROM note_streams \
         WHERE namespace = ?1 ORDER BY stream, seq",
    )?;
    let rows = stmt.query_map([source], |row| {
        Ok(StreamMember {
            note_id: row.get(0)?,
            stream: row.get(1)?,
            seq: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// Everything that can refuse before a single row is written.
///
/// Ordered by how much the caller can do about it: a malformed request first,
/// then a request the store refuses, then a request the store would corrupt.
pub fn validate(
    conn: &Connection,
    census: &NamespaceCensus,
    request: &MoveRequest,
) -> Result<(), MoveError> {
    let mut seen = BTreeSet::new();
    for route in &request.routes {
        if let SubjectClass::EdgeRelation(relation) = &route.class {
            if !khive_types::EdgeRelation::VALID_NAMES.contains(&relation.as_str()) {
                return Err(MoveError::UnknownSubjectClass {
                    key: route.class.render(),
                });
            }
        }
        if !seen.insert(route.class.clone()) {
            return Err(MoveError::DuplicateRoute {
                class: route.class.render(),
            });
        }
        if route.target == request.source {
            return Err(MoveError::TargetIsSource {
                class: route.class.render(),
            });
        }
    }

    let inventory = read_source(conn, census, &request.source)?;

    if let Some((table, rows)) = inventory.unknown.first() {
        return Err(MoveError::UnknownTable {
            table: table.clone(),
            rows: *rows,
        });
    }

    let unrouted = |class: SubjectClass, rows: u64| -> Result<(), MoveError> {
        if rows > 0 && request.route_for(&class).is_none() {
            return Err(MoveError::UnroutedClass {
                class: class.render(),
                rows,
            });
        }
        Ok(())
    };
    for (kind, rows) in &inventory.note_kinds {
        unrouted(SubjectClass::Note(kind.clone()), *rows)?;
    }
    for (kind, rows) in &inventory.entity_kinds {
        unrouted(SubjectClass::Entity(kind.clone()), *rows)?;
    }
    for (relation, rows) in &inventory.edge_relations {
        unrouted(SubjectClass::EdgeRelation(relation.clone()), *rows)?;
    }
    unrouted(SubjectClass::Atom, inventory.atoms)?;
    unrouted(SubjectClass::Domain, inventory.domains)?;

    let pinned = stream_members(conn, &request.source)?;
    if !pinned.is_empty() {
        return Err(MoveError::StreamMembers { notes: pinned });
    }

    validate_partitioned_vector_moves(conn, census, request)?;
    Ok(())
}

fn validate_partitioned_vector_moves(
    conn: &Connection,
    census: &NamespaceCensus,
    request: &MoveRequest,
) -> Result<(), MoveError> {
    if request.single_target().is_some() {
        return Ok(());
    }
    let (selector, parameters) = routed_subjects(request);
    for table in census
        .tables
        .iter()
        .filter(|table| is_runtime_vector_table(table))
    {
        let unresolved = conn
            .query_row(
                &format!(
                    "WITH routed AS ({selector}) \
                     SELECT vector.subject_id, COUNT(DISTINCT routed.target) \
                     FROM {} AS vector LEFT JOIN routed ON routed.subject_id = vector.subject_id \
                     WHERE vector.namespace = ?1 GROUP BY vector.subject_id \
                     HAVING COUNT(DISTINCT routed.target) != 1 \
                     ORDER BY vector.subject_id LIMIT 1",
                    namespace_census::quote_ident(&table.name)
                ),
                rusqlite::params_from_iter(&parameters),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?;
        if let Some((subject_id, destinations)) = unresolved {
            return Err(MoveError::UnroutableVector {
                subject_id,
                table: table.name.clone(),
                destinations,
            });
        }
    }
    Ok(())
}

/// Collisions the pre-flight can enumerate exactly, for one constraint.
///
/// Exactness is the admission criterion, not coverage. A constraint qualifies
/// only when every key column has a name and the index is not partial, because
/// those are the two cases where the query below means precisely what the
/// constraint means:
///
/// - an expression key column is reported by `PRAGMA index_xinfo` with a null
///   name, so there is nothing to join on;
/// - a partial index carries a `WHERE` clause that `index_xinfo` does not
///   report, so a join ignoring it reports clashes the constraint would not have
///   raised.
///
/// The second rule covers two cases that are not alike, and saying so here keeps
/// the comment from presenting one reason for both.
/// `idx_comm_message_external_id` (V42's channel-scoped successor to the V5
/// index) is unreachable either way: its third key column is
/// `json_extract(properties, '$.external_id')`, followed by two channel
/// expressions, and its predicate also reads the external ID. Neither the key
/// nor the filter can be expressed without evaluating it on both sides.
/// `idx_notes_namespace_kind_key` (`sql/028-notes-key.sql:4`) is not like that at
/// all: three plain column names and `WHERE key IS NOT NULL AND deleted_at IS
/// NULL`, which a source/target join CAN express exactly. It is excluded only
/// because `index_xinfo` does not hand over the `WHERE`, and it is the collision
/// a consolidation of two namespaces is most likely to hit, since two notes
/// sharing a key under one kind is the ordinary case rather than the exotic one.
/// Admitting it means deciding which predicate shapes a parse may accept, and a
/// predicate read permissively would refuse moves SQLite allows, so it is left
/// out until that rule exists rather than guessed at here.
///
/// What the excluded constraints get instead is the constraint itself: the move
/// issues plain statements, they error, and the caller's transaction rolls back.
/// So this function decides the QUALITY of a refusal, never whether one happens.
/// A collision it cannot enumerate still aborts the move.
///
/// That refusal is thinner than it sounds, and it is worth writing down because
/// it is the reason the exclusion above costs something. SQLite names the
/// COLUMNS, not the index: a caller whose note-key move is refused receives
/// `UNIQUE constraint failed: notes.namespace, notes.kind, notes.key` and gets
/// no index name to look up and no rows. Measured.
///
/// The reverse also happens, and it does not show up here at all: a constraint
/// this function DOES enumerate can be unreachable because of one the census
/// never reported. `graph_edges` is `PRIMARY KEY (namespace, id)`, which names
/// `namespace` and so arrives here as a live key — but
/// `sql/014-graph-edges-id-unique.sql:25` puts a UNIQUE index on `id` alone,
/// globally, so no two rows in the database can share an `id` and the clash this
/// arm looks for cannot exist in any store that reached V13. That index names no
/// namespace, so a census keyed on the column cannot see it, and nothing in the
/// enumerated set says the arm is dead. `idx_graph_edges_unique_triple`
/// (`namespace, source_id, target_id, relation`) is the constraint that actually
/// refuses a graph edge move, and it is enumerated here.
fn collisions_for(
    conn: &Connection,
    constraint: &NamespaceConstraint,
    source: &str,
    target: &str,
) -> rusqlite::Result<Vec<Collision>> {
    if constraint.partial || !constraint.columns_are_nameable() {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = constraint
        .columns
        .iter()
        .filter_map(|c| c.as_deref())
        .collect();
    let others: Vec<&str> = names
        .iter()
        .copied()
        .filter(|c| !c.eq_ignore_ascii_case("namespace"))
        .collect();
    if others.len() != names.len() - 1 {
        // `namespace` is not in this constraint, so moving cannot collide on it.
        return Ok(Vec::new());
    }
    if others.is_empty() {
        // A uniqueness constraint on `namespace` alone: one row per namespace,
        // and a second one arriving is a collision whatever its other columns.
        // Handled by the same query with an empty key rendering.
        return collisions_on_namespace_alone(conn, constraint, source, target);
    }

    let table = namespace_census::quote_ident(&constraint.table);
    let join = others
        .iter()
        .map(|c| {
            let q = namespace_census::quote_ident(c);
            // `IS` rather than `=` so two NULLs in a nullable key column compare
            // equal, which is what a UNIQUE index does NOT do. This over-reports
            // in exactly one direction and the direction is the safe one.
            format!("target.{q} IS source.{q}")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let select = others
        .iter()
        .map(|c| format!("source.{}", namespace_census::quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {select} FROM {table} AS source \
         JOIN {table} AS target ON target.namespace = ?2 AND {join} \
         WHERE source.namespace = ?1"
    );

    let mut stmt = conn.prepare(&sql)?;
    let column_count = others.len();
    let rows = stmt.query_map([source, target], move |row| {
        let mut parts = Vec::with_capacity(column_count);
        for i in 0..column_count {
            parts.push(match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                rusqlite::types::ValueRef::Real(v) => v.to_string(),
                rusqlite::types::ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                rusqlite::types::ValueRef::Blob(_) => "<blob>".to_string(),
            });
        }
        Ok(parts.join(", "))
    })?;

    let mut found = Vec::new();
    for key in rows {
        found.push(Collision {
            table: constraint.table.clone(),
            constraint: constraint.index.clone(),
            target: target.to_string(),
            key: key?,
        });
    }
    Ok(found)
}

fn collisions_on_namespace_alone(
    conn: &Connection,
    constraint: &NamespaceConstraint,
    source: &str,
    target: &str,
) -> rusqlite::Result<Vec<Collision>> {
    let table = namespace_census::quote_ident(&constraint.table);
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM {table} WHERE namespace = ?1) \
              * (SELECT COUNT(*) FROM {table} WHERE namespace = ?2)"
    );
    let product: i64 = conn.query_row(&sql, [source, target], |row| row.get(0))?;
    Ok(if product > 0 {
        vec![Collision {
            table: constraint.table.clone(),
            constraint: constraint.index.clone(),
            target: target.to_string(),
            key: "(namespace alone)".to_string(),
        }]
    } else {
        Vec::new()
    })
}

/// Move one note or entity kind, and the rows derived from it.
///
/// The derived writes run AFTER the base update and select through it, so no id
/// list is ever held in memory and a large namespace costs the same as a small
/// one. They are still exact: the `WHERE namespace = :source` on the derived
/// table excludes rows that were already in the target before this ran.
/// The three tables a note or entity kind is spread across.
///
/// Grouped rather than passed as three strings because they are one fact: the
/// map is keyed on the rowid the fts table holds, so naming them apart invites
/// a call site that pairs a base with the wrong map.
struct KindedTables {
    base: &'static str,
    fts: &'static str,
    rowids: &'static str,
}

const NOTE_TABLES: KindedTables = KindedTables {
    base: "notes",
    fts: "fts_notes",
    rowids: "fts_notes_rowids",
};

const ENTITY_TABLES: KindedTables = KindedTables {
    base: "entities",
    fts: "fts_entities",
    rowids: "fts_entities_rowids",
};

fn move_kinded_subject(
    conn: &Connection,
    tables: &KindedTables,
    source: &str,
    target: &str,
    kind: &str,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<u64> {
    let KindedTables { base, fts, rowids } = *tables;
    // Entity writers advance revisions explicitly; note revisions belong to
    // their update trigger. Keep the entity target and both assignment lists
    // literal so the two writer contracts can be inspected independently.
    let statement = if base == "entities" {
        "UPDATE entities SET namespace = ?2, version = version + 1 \
         WHERE namespace = ?1 AND kind = ?3"
            .to_owned()
    } else {
        format!(
            "UPDATE {} SET namespace = ?2 WHERE namespace = ?1 AND kind = ?3",
            namespace_census::quote_ident(base)
        )
    };
    let moved = conn.execute(&statement, rusqlite::params![source, target, kind])? as u64;
    *rows.entry(base.to_string()).or_default() += moved;

    // An ordinary fts5 table accepts this and preserves the rowid, which is what
    // the map below is keyed on. Measured; the arm lives in the tests.
    let selector = format!(
        "SELECT id FROM {} WHERE namespace = ?2 AND kind = ?3",
        namespace_census::quote_ident(base)
    );
    for derived in [fts, rowids] {
        let n = conn.execute(
            &format!(
                "UPDATE {} SET namespace = ?2 \
                 WHERE namespace = ?1 AND subject_id IN ({selector})",
                namespace_census::quote_ident(derived)
            ),
            rusqlite::params![source, target, kind],
        )? as u64;
        *rows.entry(derived.to_string()).or_default() += n;
    }
    Ok(moved)
}

/// Move a whole table's rows out of the source namespace.
fn move_whole_table(
    conn: &Connection,
    table: &str,
    source: &str,
    target: &str,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<u64> {
    let moved = conn.execute(
        &format!(
            "UPDATE {} SET namespace = ?2 WHERE namespace = ?1",
            namespace_census::quote_ident(table)
        ),
        rusqlite::params![source, target],
    )? as u64;
    *rows.entry(table.to_string()).or_default() += moved;
    Ok(moved)
}

fn move_edges(
    conn: &Connection,
    request: &MoveRequest,
    route: &MoveRoute,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<u64> {
    let mut parameters: Vec<rusqlite::types::Value> =
        vec![request.source.clone().into(), route.target.clone().into()];
    let predicate = if let SubjectClass::EdgeRelation(relation) = &route.class {
        parameters.push(relation.clone().into());
        "relation = ?3".to_owned()
    } else {
        let mut excluded = Vec::new();
        for specific in &request.routes {
            if let SubjectClass::EdgeRelation(relation) = &specific.class {
                parameters.push(relation.clone().into());
                excluded.push(format!("?{}", parameters.len()));
            }
        }
        if excluded.is_empty() {
            return move_whole_table(conn, "graph_edges", &request.source, &route.target, rows);
        }
        format!("relation NOT IN ({})", excluded.join(", "))
    };
    let moved = conn.execute(
        &format!("UPDATE graph_edges SET namespace = ?2 WHERE namespace = ?1 AND {predicate}"),
        rusqlite::params_from_iter(parameters),
    )? as u64;
    *rows.entry("graph_edges".into()).or_default() += moved;
    Ok(moved)
}

/// Carry a note's visibility receipt and fences without breaking their composite
/// foreign key. Copy the receipt under the target first, re-key its fences, then
/// remove the source receipt. The counts describe rows carried, not the extra
/// insert and delete needed to preserve the reference at each statement.
fn move_memory_visibility(
    conn: &Connection,
    source: &str,
    target: &str,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<()> {
    let receipts = conn.execute(
        "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) \
         SELECT ?2, receipt.note_id, receipt.model_count \
         FROM memory_visibility_receipts AS receipt \
         JOIN notes AS note ON note.id = receipt.note_id \
         WHERE receipt.namespace = ?1 AND note.namespace = ?2",
        rusqlite::params![source, target],
    )? as u64;
    *rows.entry(MEMORY_VISIBILITY_RECEIPTS.into()).or_default() += receipts;

    let fences = conn.execute(
        "UPDATE memory_visibility_fences SET namespace = ?2 \
         WHERE namespace = ?1 AND note_id IN (\
           SELECT note_id FROM memory_visibility_receipts WHERE namespace = ?2)",
        rusqlite::params![source, target],
    )? as u64;
    *rows.entry(MEMORY_VISIBILITY_FENCES.into()).or_default() += fences;

    let removed = conn.execute(
        "DELETE FROM memory_visibility_receipts \
         WHERE namespace = ?1 AND note_id IN (\
           SELECT note_id FROM memory_visibility_receipts WHERE namespace = ?2)",
        rusqlite::params![source, target],
    )? as u64;
    debug_assert_eq!(removed, receipts);
    Ok(())
}

/// A vec0 row moves by delete and re-insert, carrying the stored embedding.
///
/// No `UPDATE` against vec0 exists anywhere in the tree, and re-embedding would
/// be wasted work rather than a safer alternative: namespace is not an input to
/// an embedding, so a re-index pass recomputes byte-identical vectors.
///
/// The order is forced by the key. `subject_id` is declared `PRIMARY KEY` and
/// does not change, so an insert issued before the delete would collide with the
/// row it is replacing. The moving rows therefore land in a temporary table
/// first. That table is `TEMP`, so it is invisible to the store and dropped with
/// the connection, and it is created and dropped inside the caller's
/// transaction with everything else.
fn move_vectors(
    conn: &Connection,
    table: &str,
    source: &str,
    target: Option<&str>,
) -> rusqlite::Result<VectorMove> {
    let quoted = namespace_census::quote_ident(table);
    let columns = "subject_id, namespace, kind, field, embedding_model, embedding";

    conn.execute_batch("DROP TABLE IF EXISTS temp.namespace_move_vectors")?;
    let staged = if let Some(target) = target {
        conn.execute(
            &format!(
                "CREATE TEMP TABLE namespace_move_vectors AS \
                 SELECT {columns}, ?2 AS target FROM {quoted} WHERE namespace = ?1"
            ),
            rusqlite::params![source, target],
        )
    } else {
        conn.execute(
            &format!(
                "CREATE TEMP TABLE namespace_move_vectors AS \
                 SELECT vector.subject_id, vector.namespace, vector.kind, vector.field, \
                        vector.embedding_model, vector.embedding, routed.target \
                 FROM {quoted} AS vector JOIN (\
                   SELECT DISTINCT subject_id, target FROM temp.namespace_move_subject_targets\
                 ) AS routed ON routed.subject_id = vector.subject_id \
                 WHERE vector.namespace = ?1"
            ),
            [source],
        )
    };
    // `CREATE TABLE ... AS SELECT` reports no row count, so the count comes from
    // the staging table itself rather than from the statement.
    staged?;
    let staged_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM temp.namespace_move_vectors",
        [],
        |r| r.get(0),
    )?;

    conn.execute(
        &format!("DELETE FROM {quoted} WHERE namespace = ?1"),
        [source],
    )?;
    let inserted = conn.execute(
        &format!(
            "INSERT INTO {quoted} ({columns}) \
             SELECT subject_id, target, kind, field, embedding_model, embedding \
             FROM temp.namespace_move_vectors"
        ),
        [],
    )? as u64;
    debug_assert_eq!(
        inserted, staged_rows as u64,
        "every staged vector is re-inserted or the move is losing embeddings"
    );

    // The sidecar is optional on databases predating vector provenance. The
    // move re-inserts vec0 bytes without capturing a new prepared input or
    // attributable write time, so discard the old provenance for the exact
    // staged subject set in the same transaction. An equal BLOB would not
    // invalidate the old digest on its own.
    let has_provenance: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'vector_provenance')",
        [],
        |row| row.get(0),
    )?;
    if has_provenance {
        let model_key = table
            .strip_prefix("vec_")
            .expect("runtime vector tables use the vec_ prefix");
        conn.execute(
            "DELETE FROM vector_provenance \
             WHERE model_key = ?1 \
               AND subject_id IN (SELECT subject_id FROM temp.namespace_move_vectors)",
            [model_key],
        )?;
    }

    // The staging table is still here because THIS is what the write log has to
    // be built from. It holds the moved rows and nothing else; the live table now
    // holds them beside whatever the target already had, and a log built by
    // reading the target back cannot tell the two apart. Measured against the
    // read-back form: a target already holding one other subject's vector
    // produced a `delete` under the source for a subject the source never held,
    // a second `upsert` for a vector that never moved, and an appended count of
    // four where two vectors' worth of instructions were owed.
    let deleted = conn.execute(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT ?1, embedding_model, kind, field, subject_id, 'delete' \
         FROM temp.namespace_move_vectors",
        [source],
    )? as u64;
    let upserts = conn
        .prepare(
            "INSERT INTO ann_write_log \
             (namespace, embedding_model, kind, field, subject_id, op) \
             SELECT target, embedding_model, kind, field, subject_id, 'upsert' \
             FROM temp.namespace_move_vectors \
             RETURNING seq, subject_id, embedding_model, kind, field",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, subject, model, kind, field) in &upserts {
        if kind == "note" && field == "note.content" {
            refresh_moved_memory_fence(conn, source, subject, model, *seq)?;
        }
    }
    let appended = deleted + upserts.len() as u64;

    conn.execute_batch("DROP TABLE temp.namespace_move_vectors")?;

    Ok(VectorMove {
        moved: inserted,
        ann_appended: appended,
    })
}

/// The receipt has not been re-keyed yet. Preserve its model set and refresh
/// only a fence whose vector was actually published by this move transaction.
fn refresh_moved_memory_fence(
    conn: &Connection,
    source: &str,
    subject: &str,
    model: &str,
    seq: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE memory_visibility_fences SET ann_write_log_seq = ?4 \
         WHERE namespace = ?1 AND note_id = ?2 AND model = ?3",
        rusqlite::params![source, subject, model, seq],
    )?;
    Ok(())
}

/// What one vector table's move did: the rows carried, and the instructions
/// appended for the ANN consumers on BOTH sides.
///
/// The write log is appended to and never rewritten: its existing entries record
/// writes that happened under the old name and are true. What a move adds is two
/// entries per moved vector, not one.
///
/// One entry is not enough and the asymmetry is easy to miss. A consumer builds
/// its index per `(namespace, embedding_model)` and advances a watermark over
/// this log. An `upsert` under the target tells the target's consumer to take
/// the vector. Nothing tells the SOURCE's consumer to drop it, so without the
/// paired `delete` that index keeps answering searches with a subject that is no
/// longer in its namespace — the same silent outcome as doing nothing to the
/// vectors at all, moved one layer out.
struct VectorMove {
    moved: u64,
    ann_appended: u64,
}

/// Move records out of one namespace, per the route map, inside the caller's
/// transaction.
///
/// DML only. The caller opened `BEGIN IMMEDIATE` and owns the commit or the
/// rollback; a nested one here is a SQLite error. Every refusal happens before
/// the first write, except the ones only a constraint can raise, and those abort
/// the caller's transaction whole.
pub fn move_namespace(conn: &Connection, request: &MoveRequest) -> Result<MoveCounts, MoveError> {
    let census = namespace_census::census(conn)?;
    validate(conn, &census, request)?;

    // Over DISTINCT targets, not over routes. `collisions_for` is a function of
    // the constraint and the two namespaces and does not read the route's class,
    // so two routes sharing a target ask the same question twice and the answers
    // are byte-identical. A `Collision` carries no route, so the repeats are not
    // a second fact about a second class, they are the same row printed again.
    // Found by the fixture, which planted three note kinds bound for one target
    // and read back the same collision three times.
    let mut targets: BTreeSet<&str> = BTreeSet::new();
    for route in &request.routes {
        targets.insert(route.target.as_str());
    }
    let mut collisions = Vec::new();
    for constraint in namespace_census::reachable_constraints(&census) {
        for target in &targets {
            collisions.extend(collisions_for(conn, constraint, &request.source, target)?);
        }
    }
    if !collisions.is_empty() {
        return Err(MoveError::Collisions { collisions });
    }

    let mut counts = MoveCounts::default();
    let source = request.source.as_str();

    // Capture the source identities before any base row moves, excluding every
    // target resident even when it has the same vector kind or route class.
    if request.single_target().is_none() {
        let (selector, parameters) = routed_subjects(request);
        conn.execute_batch("DROP TABLE IF EXISTS temp.namespace_move_subject_targets")?;
        conn.execute(
            &format!("CREATE TEMP TABLE namespace_move_subject_targets AS {selector}"),
            rusqlite::params_from_iter(parameters),
        )?;
    }

    // Audit rows are selected by the routed note kind, so they go before the
    // notes do.
    move_task_audit(conn, &census, request, &mut counts.rows)?;

    for route in &request.routes {
        let target = route.target.as_str();
        let moved = match &route.class {
            SubjectClass::Note(kind) => {
                move_kinded_subject(conn, &NOTE_TABLES, source, target, kind, &mut counts.rows)?
            }
            SubjectClass::Entity(kind) => {
                move_kinded_subject(conn, &ENTITY_TABLES, source, target, kind, &mut counts.rows)?
            }
            SubjectClass::Edge | SubjectClass::EdgeRelation(_) => {
                move_edges(conn, request, route, &mut counts.rows)?
            }
            SubjectClass::Atom => {
                move_knowledge_atoms(conn, request, target, false, &mut counts.rows)?
            }
            SubjectClass::Domain => {
                // A domain and its same-ID mirror atom form one logical subject.
                move_knowledge_atoms(conn, request, target, true, &mut counts.rows)?;
                move_whole_table(conn, "knowledge_domains", source, target, &mut counts.rows)?
            }
        };
        counts.subjects.insert(route.class.render(), moved);
    }

    // Single-target vectors carry every source section's vector, including
    // historical sections with a missing or differently attributed parent.
    if let Some(target) = request.single_target() {
        move_whole_table(conn, "knowledge_sections", source, target, &mut counts.rows)?;
    }

    // Vectors are enumerated from the live store, never from a constant list: a
    // store using a model this build was never compiled against still has its
    // `vec_*` table found here.
    for table in &census.tables {
        if !is_runtime_vector_table(table) {
            continue;
        }
        let moved = move_vectors(conn, &table.name, source, request.single_target())?;
        *counts.rows.entry(table.name.clone()).or_default() += moved.moved;
        counts.ann_log_appended += moved.ann_appended;
    }
    if request.single_target().is_none() {
        conn.execute_batch("DROP TABLE temp.namespace_move_subject_targets")?;
    }
    if census
        .tables
        .iter()
        .any(|table| table.name == "vector_provenance")
    {
        let left = count_in_namespace(conn, "vector_provenance", source)?;
        if left > 0 {
            counts.left_behind.insert("vector_provenance".into(), left);
        }
    }

    // Learned state follows the subject it is about. Run per distinct target, so
    // a partitioning move sends each row after the subject it names. The set is
    // the one the pre-flight above already built, for the same reason.
    for table in SUBJECT_KEYED_TABLES {
        for target in &targets {
            let moved = conn.execute(
                &format!(
                    "UPDATE {} SET namespace = ?2 WHERE namespace = ?1 AND target_id IN (\
                       SELECT id FROM notes WHERE namespace = ?2 \
                       UNION ALL SELECT id FROM entities WHERE namespace = ?2 \
                       UNION ALL SELECT id FROM knowledge_atoms WHERE namespace = ?2)",
                    namespace_census::quote_ident(table)
                ),
                rusqlite::params![source, target],
            )? as u64;
            *counts.rows.entry((*table).to_string()).or_default() += moved;
        }
        let left = count_in_namespace(conn, table, source)?;
        if left > 0 {
            counts.left_behind.insert((*table).to_string(), left);
        }
    }

    // Visibility receipts follow notes, but their fences reference the receipt
    // including its namespace. A plain UPDATE of either table first would fail
    // with foreign keys enabled. The helper keeps the reference valid throughout.
    for target in &targets {
        move_memory_visibility(conn, source, target, &mut counts.rows)?;
    }
    for table in [MEMORY_VISIBILITY_RECEIPTS, MEMORY_VISIBILITY_FENCES] {
        let left = count_in_namespace(conn, table, source)?;
        if left > 0 {
            counts.left_behind.insert((*table).to_string(), left);
        }
    }

    for table in LEAVE_BEHIND_TABLES {
        let left = count_in_namespace(conn, table, source)?;
        if left > 0 {
            counts.left_behind.insert((*table).to_string(), left);
        }
    }

    // Per-namespace aggregates with no subject. A partitioning move has no
    // target to carry them to, so they stay and the caller is told, rather than
    // being left to find out.
    for table in NAMESPACE_SCOPED_TABLES {
        match request.single_target() {
            Some(target) => {
                move_whole_table(conn, table, source, target, &mut counts.rows)?;
            }
            None => {
                let left = count_in_namespace(conn, table, source)?;
                if left > 0 {
                    counts.left_behind.insert((*table).to_string(), left);
                }
            }
        }
    }

    settle_pack_tables(conn, &census, request, &mut counts)?;

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::run_migrations;
    use rusqlite::Connection;

    pub(super) fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open");
        run_migrations(&mut conn).expect("migrate");
        conn
    }

    /// Seeds through raw SQL, which is enough for every arm below and is NOT
    /// enough for the ones that are deliberately absent.
    ///
    /// `fts_notes` and `fts_entities` have no triggers: their contents are
    /// written from Rust, so a SQL seed leaves them empty and an arm asserting
    /// the move carried them would pass against a store where there was nothing
    /// to carry. Those arms need a fixture built through the store's own
    /// writers. `fts_knowledge` and `fts_sections` ARE trigger-maintained, so
    /// they are reachable from here and are exercised.
    pub(super) fn seed_note(conn: &Connection, id: &str, namespace: &str, kind: &str) {
        conn.execute(
            "INSERT INTO notes (id, namespace, kind, name, content, created_at, updated_at) \
             VALUES (?1, ?2, ?3, 'a name', 'some content', 1, 1)",
            rusqlite::params![id, namespace, kind],
        )
        .expect("seed note");
    }

    pub(super) fn route(key: &str, target: &str) -> MoveRoute {
        MoveRoute {
            class: SubjectClass::parse(key).expect("route key"),
            target: target.to_string(),
        }
    }

    /// The property the whole multi-backend story rests on. A store split across
    /// several SQLite files runs the same request once per backend, and that
    /// composes only because a backend holding none of a routed class is a
    /// SUCCESS reporting zero rather than a refusal.
    #[test]
    fn a_routed_class_with_no_rows_succeeds_reporting_zero() {
        let conn = migrated();
        let request = MoveRequest::new(
            "empty-source",
            vec![route("note:observation", "target"), route("atom", "target")],
        );
        let counts = move_namespace(&conn, &request).expect("a backend with nothing routed here");
        assert_eq!(counts.subjects.get("note:observation"), Some(&0));
        assert_eq!(counts.subjects.get("atom"), Some(&0));
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_RECEIPTS), Some(&0));
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_FENCES), Some(&0));
    }

    #[test]
    fn a_memory_visibility_receipt_follows_its_note_and_reports_count() {
        let conn = migrated();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute(
            "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) \
             VALUES ('source', 'n1', 0)",
            [],
        )
        .unwrap();

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let counts = move_namespace(&conn, &request).expect("the receipt follows its note");
        assert_eq!(counts.subjects.get("note:observation"), Some(&1));
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_RECEIPTS), Some(&1));
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_FENCES), Some(&0));
        let stored: (String, i64) = conn
            .query_row(
                "SELECT namespace, model_count FROM memory_visibility_receipts \
                 WHERE note_id = 'n1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, ("target".into(), 0));
        let source_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_visibility_receipts WHERE namespace = 'source'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_rows, 0);
    }

    #[test]
    fn memory_visibility_fences_follow_their_note_with_foreign_keys_enabled() {
        let conn = migrated();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        seed_note(&conn, "n1", "source", "observation");
        seed_note(&conn, "n2", "source", "decision");
        conn.execute(
            "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) \
             VALUES ('source', 'n1', 2), ('source', 'n2', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_visibility_fences \
             (namespace, note_id, model, ann_write_log_seq) VALUES \
             ('source', 'n1', 'model-a', 10), ('source', 'n1', 'model-b', 11)",
            [],
        )
        .unwrap();

        let request = MoveRequest::new(
            "source",
            vec![
                route("note:observation", "target-a"),
                route("note:decision", "target-b"),
            ],
        );
        let counts = move_namespace(&conn, &request).expect("both receipts follow their notes");
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_RECEIPTS), Some(&2));
        assert_eq!(counts.rows.get(MEMORY_VISIBILITY_FENCES), Some(&2));
        let receipt_places: Vec<(String, String)> = conn
            .prepare("SELECT note_id, namespace FROM memory_visibility_receipts ORDER BY note_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            receipt_places,
            vec![
                ("n1".into(), "target-a".into()),
                ("n2".into(), "target-b".into())
            ]
        );
        let fences: Vec<(String, String, i64)> = conn
            .prepare(
                "SELECT namespace, model, ann_write_log_seq \
                 FROM memory_visibility_fences ORDER BY model",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            fences,
            vec![
                ("target-a".into(), "model-a".into(), 10),
                ("target-a".into(), "model-b".into(), 11)
            ]
        );
        let foreign_key_errors: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_errors, 0);
    }

    #[cfg(feature = "vectors")]
    #[test]
    fn moved_visibility_fences_use_each_exact_upsert_and_preserve_unrelated_receipts() {
        crate::extension::ensure_extensions_loaded();
        let mut conn = migrated();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (id, namespace) in [
            ("moved", "source"),
            ("zero", "source"),
            ("unfenced", "source"),
            ("existing", "target"),
        ] {
            seed_note(&conn, id, namespace, "memory");
        }
        conn.execute_batch(
            "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) VALUES \
             ('source', 'moved', 2), ('source', 'zero', 0), ('target', 'existing', 1); \
             INSERT INTO memory_visibility_fences (namespace, note_id, model, ann_write_log_seq) VALUES \
             ('source', 'moved', 'model-a', 17), ('source', 'moved', 'model-b', 18), \
             ('target', 'existing', 'model-a', 99);",
        ).unwrap();
        for (table, model) in [("vec_model_a", "model-a"), ("vec_model_b", "model-b")] {
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE {table} USING vec0(\
                 subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                 kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
                 embedding float[2] distance_metric=cosine)"
            ))
            .unwrap();
            conn.execute(&format!(
                "INSERT INTO {table} (subject_id, namespace, kind, field, embedding_model, embedding) \
                 VALUES ('moved', 'source', 'note', 'note.content', ?1, '[0.1, 0.2]')"
            ), [model]).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO vec_model_a (subject_id, namespace, kind, field, embedding_model, embedding) VALUES \
             ('unfenced', 'source', 'note', 'note.content', 'model-a', '[0.1, 0.2]'), \
             ('existing', 'target', 'note', 'note.content', 'model-a', '[0.1, 0.2]'); \
             INSERT INTO ann_write_log (seq, namespace, embedding_model, kind, field, subject_id, op) \
             VALUES (100, 'target', 'model-a', 'note', 'note.content', 'existing', 'upsert');",
        ).unwrap();
        let transaction = conn.transaction().unwrap();
        let counts = move_namespace(
            &transaction,
            &MoveRequest::new("source", vec![route("note:memory", "target")]),
        )
        .expect("move memory receipts and both vector models");
        assert_eq!(counts.ann_log_appended, 6);
        for model in ["model-a", "model-b"] {
            let fence: i64 = transaction
                .query_row(
                    "SELECT ann_write_log_seq FROM memory_visibility_fences \
                 WHERE namespace = 'target' AND note_id = 'moved' AND model = ?1",
                    [model],
                    |row| row.get(0),
                )
                .unwrap();
            let upsert: i64 = transaction
                .query_row(
                    "SELECT seq FROM ann_write_log WHERE namespace = 'target' \
                 AND subject_id = 'moved' AND embedding_model = ?1 AND op = 'upsert'",
                    [model],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(upsert > 100);
            assert_eq!(fence, upsert, "each model owes its own destination upsert");
        }
        let untouched: i64 = transaction
            .query_row(
                "SELECT ann_write_log_seq FROM memory_visibility_fences \
             WHERE namespace = 'target' AND note_id = 'existing' AND model = 'model-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(untouched, 99);
        let zero_count: i64 = transaction
            .query_row(
                "SELECT model_count FROM memory_visibility_receipts \
             WHERE namespace = 'target' AND note_id = 'zero'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(zero_count, 0);
        let invented: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM memory_visibility_fences WHERE note_id IN ('zero', 'unfenced')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(invented, 0);
        transaction.commit().unwrap();
    }

    /// And the case it must stay distinguishable from. A host binding a pack
    /// that writes nothing needs "routed, nothing there" to read differently
    /// from "you forgot this one".
    #[test]
    fn a_class_with_rows_and_no_route_refuses_and_says_how_many() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        seed_note(&conn, "n2", "source", "decision");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let error = move_namespace(&conn, &request).expect_err("decision notes are unrouted");
        match error {
            MoveError::UnroutedClass { class, rows } => {
                assert_eq!(class, "note:decision");
                assert_eq!(rows, 1);
            }
            other => panic!("expected an unrouted class, got {other}"),
        }

        let still_here: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE namespace = 'source'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(still_here, 2, "a refusal writes nothing");
    }

    /// The drift guard, and the reason `disposition` returning `None` is a
    /// refusal rather than a default. This arm mints a namespace-bearing table
    /// the way a migration would, so it passes only if the refusal is derived
    /// from the census rather than from a list somebody remembered to edit.
    #[test]
    fn a_namespace_table_this_build_has_no_rule_for_refuses_the_move() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute_batch(
            "CREATE TABLE later_migration_added_this (\
               id TEXT PRIMARY KEY, namespace TEXT NOT NULL);\
             INSERT INTO later_migration_added_this VALUES ('x', 'source');",
        )
        .expect("a migration lands");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let error = move_namespace(&conn, &request).expect_err("an unknown table refuses");
        match error {
            MoveError::UnknownTable { table, rows } => {
                assert_eq!(table, "later_migration_added_this");
                assert_eq!(rows, 1);
            }
            other => panic!("expected an unknown table, got {other}"),
        }
    }

    /// The same refusal, for a table whose NAME says it is a vector table.
    ///
    /// The vector tables are created at runtime by embedding models and appear in
    /// no source file, so they are recognised from the live store. Recognising
    /// them by name alone puts a hole through the refusal above: a migration
    /// adding an ordinary table called `vec_audit` would be classed as a vector
    /// table, handed to the vector move, and die on `no such column: embedding`
    /// somewhere inside the caller's transaction. That is the one outcome the
    /// refusal exists to prevent -- a bare SQLite error in place of a named
    /// refusal. A real vector table is a `CREATE VIRTUAL TABLE` and this one is
    /// not, which is what separates them here.
    #[test]
    fn a_table_named_like_a_vector_table_but_not_one_refuses_by_name() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute_batch(
            "CREATE TABLE vec_audit (\
               id TEXT PRIMARY KEY, namespace TEXT NOT NULL);\
             INSERT INTO vec_audit VALUES ('x', 'source');",
        )
        .expect("a migration lands a table whose name starts with the prefix");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let error = move_namespace(&conn, &request).expect_err("the prefix is not enough");
        match error {
            MoveError::UnknownTable { table, rows } => {
                assert_eq!(table, "vec_audit");
                assert_eq!(rows, 1);
            }
            other => panic!("expected an unknown table, got {other}"),
        }
    }

    /// Control for the arm above: the same table with no rows in the source
    /// namespace does NOT refuse, because a move that never touches it has
    /// nothing to be wrong about.
    #[test]
    fn an_unknown_table_holding_nothing_here_does_not_refuse() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute_batch(
            "CREATE TABLE later_migration_added_this (\
               id TEXT PRIMARY KEY, namespace TEXT NOT NULL);\
             INSERT INTO later_migration_added_this VALUES ('x', 'somewhere-else');",
        )
        .expect("a migration lands");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let counts = move_namespace(&conn, &request).expect("nothing of ours is in that table");
        assert_eq!(counts.subjects.get("note:observation"), Some(&1));
    }

    #[test]
    fn a_route_to_the_namespace_it_is_already_in_refuses() {
        let conn = migrated();
        let request = MoveRequest::new("source", vec![route("atom", "source")]);
        match move_namespace(&conn, &request).expect_err("a no-op written as an instruction") {
            MoveError::TargetIsSource { class } => assert_eq!(class, "atom"),
            other => panic!("expected target-is-source, got {other}"),
        }
    }

    #[test]
    fn the_same_class_routed_twice_refuses_rather_than_picking_one() {
        let conn = migrated();
        let request = MoveRequest::new(
            "source",
            vec![route("atom", "one"), route("atom", "another")],
        );
        match move_namespace(&conn, &request).expect_err("two targets, no rule to choose") {
            MoveError::DuplicateRoute { class } => assert_eq!(class, "atom"),
            other => panic!("expected a duplicate route, got {other}"),
        }
    }

    #[test]
    fn an_unknown_route_key_names_what_it_was_given() {
        match SubjectClass::parse("notes:observation").expect_err("plural is a typo") {
            MoveError::UnknownSubjectClass { key } => assert_eq!(key, "notes:observation"),
            other => panic!("expected an unknown class, got {other}"),
        }
        assert_eq!(
            SubjectClass::parse("note:observation").expect("singular"),
            SubjectClass::Note("observation".into())
        );
    }

    /// A partitioning move has no target to carry a per-namespace aggregate to,
    /// so it stays AND is reported. Silence here would be the failure: the seat
    /// that eventually binds brain would discover the rows instead of reading a
    /// line about them.
    #[test]
    fn a_partitioning_move_reports_the_aggregates_it_leaves_behind() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        seed_note(&conn, "n2", "source", "decision");
        conn.execute(
            "INSERT INTO brain_profile_snapshots (profile_id, namespace, snapshot_json, updated_at) \
             VALUES ('p', 'source', '{}', 1)",
            [],
        )
        .expect("seed a snapshot");

        let request = MoveRequest::new(
            "source",
            vec![
                route("note:observation", "one"),
                route("note:decision", "another"),
            ],
        );
        let counts = move_namespace(&conn, &request).expect("a partitioning move");
        assert_eq!(counts.left_behind.get("brain_profile_snapshots"), Some(&1));

        let stayed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM brain_profile_snapshots WHERE namespace = 'source'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(stayed, 1);
    }

    /// The same aggregate DOES move when every route names one target, because
    /// then there is somewhere for it to belong.
    #[test]
    fn a_total_single_target_move_carries_the_aggregates() {
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute(
            "INSERT INTO brain_profile_snapshots (profile_id, namespace, snapshot_json, updated_at) \
             VALUES ('p', 'source', '{}', 1)",
            [],
        )
        .expect("seed a snapshot");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let counts = move_namespace(&conn, &request).expect("a total move");
        assert!(counts.left_behind.is_empty(), "{:?}", counts.left_behind);

        let moved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM brain_profile_snapshots WHERE namespace = 'target'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(moved, 1);
    }
    /// The half of a vector move that is invisible from the side it leaves.
    ///
    /// An ANN consumer builds its index per `(namespace, embedding_model)` by
    /// advancing a watermark over `ann_write_log`. Moving the row in the `vec_*`
    /// table and appending only the target's `upsert` leaves the source's index
    /// intact and still answering searches with a subject that is no longer in
    /// its namespace, which is the same observable as never having touched the
    /// vectors at all. This arm fails if nothing tells the source side to drop
    /// what left.
    ///
    /// The consumer itself lives above this crate, so what is asserted here is
    /// the instruction it reads, not the index it builds from it.
    #[cfg(feature = "vectors")]
    #[test]
    fn a_vector_move_tells_the_source_side_to_drop_what_left() {
        // Registration is an auto-extension, so it only reaches connections
        // opened after it. This has to come before `migrated`.
        crate::extension::ensure_extensions_loaded();
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_test_model USING vec0(\
               subject_id TEXT PRIMARY KEY, \
               namespace TEXT NOT NULL, \
               kind TEXT NOT NULL, \
               field TEXT NOT NULL, \
               embedding_model TEXT NOT NULL, \
               embedding float[4] distance_metric=cosine\
             )",
        )
        .expect("the vector table an embedding model creates at runtime");
        conn.execute(
            "INSERT INTO vec_test_model \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES ('n1', 'source', 'observation', 'content', 'test-model', \
                     '[0.1, 0.2, 0.3, 0.4]')",
            [],
        )
        .expect("seed a vector");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let counts = move_namespace(&conn, &request).expect("a total move");

        assert_eq!(
            counts.rows.get("vec_test_model"),
            Some(&1),
            "the vector itself moved"
        );
        let left_in_source: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vec_test_model WHERE namespace = 'source'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(left_in_source, 0);

        // Two entries per moved vector, and the one that matters here is the
        // first: without it the source's index is never told anything.
        assert_eq!(counts.ann_log_appended, 2);
        let dropped_from_source: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log \
                 WHERE namespace = 'source' AND op = 'delete' \
                   AND subject_id = 'n1' AND embedding_model = 'test-model'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(
            dropped_from_source, 1,
            "the source consumer is never told to drop the vector, so its index \
             keeps answering with a subject that has left the namespace"
        );
        let taken_by_target: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log \
                 WHERE namespace = 'target' AND op = 'upsert' \
                   AND subject_id = 'n1' AND embedding_model = 'test-model'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(taken_by_target, 1);
    }

    #[cfg(feature = "vectors")]
    #[tokio::test]
    async fn vector_provenance_namespace_move_clears_sidecar() {
        use std::sync::Arc;

        use khive_storage::VectorStore;

        use crate::pool::{ConnectionPool, PoolConfig};
        use crate::stores::vectors::SqliteVecStore;

        crate::extension::ensure_extensions_loaded();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("namespace-move-provenance.db");
        let mut conn = Connection::open(&path).expect("open database");
        run_migrations(&mut conn).expect("migrate database");
        let subject_id = uuid::Uuid::new_v4();
        let subject = subject_id.to_string();
        seed_note(&conn, &subject, "source", "observation");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_test_model USING vec0(\
             subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, embedding float[2] distance_metric=cosine)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_test_model \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES (?1, 'source', 'observation', 'content', 'test-model', '[0.1, 0.2]')",
            [&subject],
        )
        .unwrap();
        let before_blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM vec_test_model WHERE subject_id = ?1",
                [&subject],
                |row| row.get(0),
            )
            .unwrap();
        let digest = blake3::hash(&before_blob).to_hex().to_string();
        conn.execute(
            "INSERT INTO vector_provenance \
             (model_key, subject_id, namespace, embedding_digest, text_fingerprint, updated_at) \
             VALUES ('test_model', ?1, 'source', ?2, ?3, '2026-09-25T12:34:56Z')",
            rusqlite::params![&subject, &digest, "a".repeat(64)],
        )
        .unwrap();

        // A target-scoped stale sidecar with equal BLOB bytes would become
        // apparently current after the move if clearing were source-scoped.
        let stale_target_id = uuid::Uuid::new_v4();
        let stale_target_subject = stale_target_id.to_string();
        seed_note(&conn, &stale_target_subject, "source", "observation");
        conn.execute(
            "INSERT INTO vec_test_model \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES (?1, 'source', 'observation', 'content', 'test-model', '[0.1, 0.2]')",
            [&stale_target_subject],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vector_provenance \
             (model_key, subject_id, namespace, embedding_digest, text_fingerprint, updated_at) \
             VALUES ('test_model', ?1, 'target', ?2, ?3, '2026-09-25T12:34:56Z')",
            rusqlite::params![&stale_target_subject, &digest, "b".repeat(64)],
        )
        .unwrap();

        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        move_namespace(
            &conn,
            &MoveRequest::new("source", vec![route("note:observation", "target")]),
        )
        .unwrap();
        conn.execute_batch("COMMIT").unwrap();
        let sidecars: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vector_provenance \
                 WHERE model_key = 'test_model' AND subject_id IN (?1, ?2)",
                rusqlite::params![&subject, &stale_target_subject],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sidecars, 0,
            "a move must invalidate even an equal-BLOB sidecar"
        );
        let after_blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM vec_test_model \
                 WHERE subject_id = ?1 AND namespace = 'target'",
                [&subject],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after_blob, before_blob, "the vector itself must survive");
        drop(conn);

        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .expect("reopen moved database"),
        );
        let vectors = SqliteVecStore::new(
            pool,
            true,
            "test_model".into(),
            "test-model".into(),
            2,
            "target".into(),
        )
        .expect("open target vector store");
        let observed = vectors
            .provenance(subject_id)
            .await
            .expect("read moved vector")
            .expect("moved vector remains present");
        assert_eq!(observed.text_fingerprint, None);
        assert_eq!(observed.updated_at, None);
        let stale_target = vectors
            .provenance(stale_target_id)
            .await
            .expect("read formerly stale target vector")
            .expect("second moved vector remains present");
        assert_eq!(stale_target.text_fingerprint, None);
        assert_eq!(stale_target.updated_at, None);
    }

    /// The instructions are about the vectors that MOVED, and a target is
    /// allowed to have vectors of its own already.
    ///
    /// Built by reading the live table back after the insert, the source side is
    /// "everything now under the target", which is the moved rows plus whatever
    /// was already there. That tells the source's consumer to drop a subject the
    /// source never held, re-upserts a vector that did not move, and reports an
    /// appended count of four where two instructions were owed. The staged rows
    /// are the only reading of "what moved" that survives the insert, which is
    /// why the log is built before they are dropped.
    #[cfg(feature = "vectors")]
    #[test]
    fn a_vector_the_target_already_held_is_not_in_the_instructions() {
        crate::extension::ensure_extensions_loaded();
        let conn = migrated();
        seed_note(&conn, "n1", "source", "observation");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_test_model USING vec0(\
               subject_id TEXT PRIMARY KEY, \
               namespace TEXT NOT NULL, \
               kind TEXT NOT NULL, \
               field TEXT NOT NULL, \
               embedding_model TEXT NOT NULL, \
               embedding float[4] distance_metric=cosine\
             )",
        )
        .expect("the vector table an embedding model creates at runtime");
        conn.execute(
            "INSERT INTO vec_test_model \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES ('n1', 'source', 'observation', 'content', 'test-model', \
                     '[0.1, 0.2, 0.3, 0.4]')",
            [],
        )
        .expect("the vector that moves");
        conn.execute(
            "INSERT INTO vec_test_model \
             (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES ('already-there', 'target', 'observation', 'content', 'test-model', \
                     '[0.5, 0.6, 0.7, 0.8]')",
            [],
        )
        .expect("a vector the target already holds");

        let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
        let counts = move_namespace(&conn, &request).expect("a total move");

        assert_eq!(
            counts.ann_log_appended, 2,
            "two instructions are owed for the one vector that moved"
        );
        let about_the_resident: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log WHERE subject_id = 'already-there'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(
            about_the_resident, 0,
            "a vector that did not move is told nothing, and is certainly not \
             dropped from a namespace it was never in"
        );
        // The control, so the arm cannot pass on a move that logged nothing at
        // all: the vector that did move still has both of its instructions.
        let about_the_mover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log WHERE subject_id = 'n1'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(about_the_mover, 2);
    }
    /// Two routes bound for one target report a shared collision once, not twice.
    ///
    /// The pre-flight reads a constraint and two namespaces and never reads the
    /// route's class, so iterating routes asked the same question once per route
    /// and pushed byte-identical rows. A `Collision` carries no route, so a
    /// repeat says nothing a reader can act on: it inflates the list in
    /// proportion to how finely the caller partitioned its request, which is the
    /// one thing the refusal should be independent of.
    ///
    /// The arm fails on the unfixed code by reporting the same collision three
    /// times, once per route. Restoring the route-keyed loop is the control.
    #[test]
    fn two_routes_to_one_target_report_a_shared_collision_once() {
        let conn = migrated();
        // The clash is on the atom slug, which is a plain two-column unique
        // index and the one collision a SQL seed can plant honestly. Every class
        // present in the source must be routed or `validate` refuses first, so
        // the source holds exactly what these three routes name.
        seed_note(&conn, "n1", "source", "observation");
        seed_note(&conn, "n2", "source", "insight");
        for (id, namespace) in [("a1", "source"), ("a2", "target")] {
            conn.execute(
                "INSERT INTO knowledge_atoms \
                 (id, namespace, slug, name, created_at, updated_at) \
                 VALUES (?1, ?2, 'shared-slug', 'an atom', 1, 1)",
                rusqlite::params![id, namespace],
            )
            .expect("seed an atom on each side of the move");
        }

        let request = MoveRequest::new(
            "source",
            vec![
                route("note:observation", "target"),
                route("note:insight", "target"),
                route("atom", "target"),
            ],
        );
        let error = move_namespace(&conn, &request).expect_err("the pre-flight refuses");
        let MoveError::Collisions { collisions } = error else {
            panic!("expected a named collision list, got {error:?}");
        };

        assert_eq!(
            collisions.len(),
            1,
            "three routes share one target, so the one blocking row is reported \
             once: {collisions:?}"
        );
        assert_eq!(collisions[0].table, "knowledge_atoms");
        assert_eq!(collisions[0].constraint, "idx_knowledge_atoms_ns_slug");
        assert_eq!(collisions[0].key, "shared-slug");
    }
}

#[cfg(test)]
#[test]
fn issue2673_namespace_move_advances_entity_version_without_changing_timestamp() {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::run_migrations(&mut conn).unwrap();
    conn.execute("INSERT INTO entities(id,namespace,kind,name,created_at,updated_at) VALUES('versioned','source','concept','moved',7,7)", []).unwrap();
    conn.execute(
        "INSERT INTO notes(id,namespace,kind,content,created_at,updated_at) \
         VALUES('note-moved','source','observation','moved',11,11), \
               ('note-control','unrelated','observation','unchanged',13,13)",
        [],
    )
    .unwrap();
    let request = MoveRequest::new(
        "source",
        vec![
            MoveRoute {
                class: SubjectClass::Entity("concept".into()),
                target: "target".into(),
            },
            MoveRoute {
                class: SubjectClass::Note("observation".into()),
                target: "target".into(),
            },
        ],
    );
    let tx = conn.transaction().unwrap();
    let moved = move_namespace(&tx, &request).unwrap();
    assert_eq!(moved.subjects.get("entity:concept"), Some(&1));
    assert_eq!(moved.subjects.get("note:observation"), Some(&1));
    tx.commit().unwrap();
    let stored = conn
        .query_row(
            "SELECT namespace,updated_at,version FROM entities WHERE id='versioned'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(stored, ("target".into(), 7, 2));
    for (id, namespace, timestamp, version) in [
        ("note-moved", "target", 11_i64, 2_i64),
        ("note-control", "unrelated", 13_i64, 1_i64),
    ] {
        let stored = conn
            .query_row(
                "SELECT namespace,updated_at,version FROM notes WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(stored, (namespace.into(), timestamp, version));
    }

    let tx = conn.transaction().unwrap();
    let repeated = move_namespace(&tx, &request).unwrap();
    assert_eq!(repeated.subjects.get("entity:concept"), Some(&0));
    assert_eq!(repeated.subjects.get("note:observation"), Some(&0));
    tx.commit().unwrap();
    for (sql, expected) in [
        ("SELECT version FROM entities WHERE id='versioned'", 2_i64),
        ("SELECT version FROM notes WHERE id='note-moved'", 2_i64),
        ("SELECT version FROM notes WHERE id='note-control'", 1_i64),
    ] {
        assert_eq!(
            conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap(),
            expected
        );
    }
}

#[cfg(all(test, feature = "vectors"))]
#[path = "namespace_move_partition_tests.rs"]
mod partition_tests;

#[cfg(test)]
#[path = "namespace_move_edge_tests.rs"]
mod edge_tests;

#[cfg(test)]
#[path = "namespace_move_pack_tables_tests.rs"]
mod pack_table_tests;

#[cfg(all(test, feature = "vectors"))]
#[path = "namespace_move_orphan_section_tests.rs"]
mod orphan_section_tests;
