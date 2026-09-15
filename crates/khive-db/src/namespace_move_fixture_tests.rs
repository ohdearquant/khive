//! What the move owes a store that contains the shapes it has to reason about.
//!
//! The arms in `namespace_move::tests` are about the route map: unknown keys,
//! duplicate routes, unrouted classes, the aggregates a partitioning move leaves
//! behind. These are about the STORE: a collision already sitting in a target,
//! an index row whose base record is gone, a constraint the census reports that
//! can never refuse anything, and the two fts5 shapes that answer the same
//! statement differently. None of them is derivable from the route map, and each
//! needs rows to exist before it can be asked.

use std::collections::BTreeSet;

use rusqlite::Connection;

use crate::migrations::run_migrations;
use crate::namespace_census::census;
use crate::namespace_move::{
    move_namespace, Collision, MoveError, MoveRequest, MoveRoute, SubjectClass,
};
use crate::namespace_move_fixture::{
    build, routes, Fixture, FixtureSpec, ATOM, ATOM_SLUG_HOLDER, EDGE, ENTITY, NOTE_DELETED,
    NOTE_OBSERVATION, NOTE_TASK, SECTION,
};

fn migrated() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory connection");
    run_migrations(&mut conn).expect("migrate to the current schema");
    conn
}

fn fixture(spec: FixtureSpec) -> (Connection, Fixture) {
    let conn = migrated();
    let built = build(&conn, &spec).expect("build the fixture");
    (conn, built)
}

fn request(built: &Fixture) -> MoveRequest {
    MoveRequest::new(
        built.spec.source.clone(),
        routes(&built.spec)
            .into_iter()
            .map(|(key, target)| MoveRoute {
                class: SubjectClass::parse(key).expect("a route key this fixture wrote"),
                target,
            })
            .collect(),
    )
}

/// Call the mover the way its caller does: inside a transaction it does not own.
///
/// The module is explicit that it opens none of its own, because the writer task
/// hands it a connection already inside `BEGIN IMMEDIATE`. An arm that skips
/// this is not testing the move, it is testing half of one — a refusal raised by
/// a constraint rather than by the pre-flight lands after earlier routes have
/// already been written, and without the enclosing transaction those writes
/// stay.
fn attempt(
    conn: &Connection,
    request: &MoveRequest,
) -> Result<crate::namespace_move::MoveCounts, MoveError> {
    conn.execute_batch("SAVEPOINT move").expect("savepoint");
    let outcome = move_namespace(conn, request);
    if outcome.is_err() {
        conn.execute_batch("ROLLBACK TO move").expect("rollback");
    }
    conn.execute_batch("RELEASE move").expect("release");
    outcome
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect(sql)
}

fn text(conn: &Connection, sql: &str) -> String {
    conn.query_row(sql, [], |row| row.get(0)).expect(sql)
}

fn collisions(outcome: Result<crate::namespace_move::MoveCounts, MoveError>) -> Vec<Collision> {
    match outcome {
        Err(MoveError::Collisions { collisions }) => collisions,
        Err(other) => panic!("expected named collisions, got {other}"),
        Ok(counts) => panic!("expected a refusal, the move succeeded: {counts:?}"),
    }
}

/// The baseline every refusal arm is read against: the same rows, no occupied
/// keys, everything carried, and the aggregates reported rather than moved.
#[test]
fn a_store_with_no_occupied_keys_moves_whole_and_counts_per_route() {
    let (conn, built) = fixture(FixtureSpec::movable("tenant:tnt_fixture"));
    let counts = attempt(&conn, &request(&built)).expect("nothing is in the way");

    assert_eq!(counts.subjects.get("note:observation").copied(), Some(2));
    assert_eq!(counts.subjects.get("note:task").copied(), Some(1));
    assert_eq!(counts.subjects.get("entity:concept").copied(), Some(1));
    assert_eq!(counts.subjects.get("edge").copied(), Some(1));
    assert_eq!(counts.subjects.get("atom").copied(), Some(1));

    let kg = built.spec.kg.clone();
    let source = built.spec.source.clone();
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{source}'")
        ),
        0,
        "the source namespace is empty of notes afterwards"
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{kg}'")
        ),
        2,
        "the soft-deleted observation moves with its live sibling: it is a \
         record, not a tombstone to be dropped"
    );
    assert_eq!(
        text(
            &conn,
            &format!("SELECT namespace FROM fts_notes WHERE subject_id = '{NOTE_OBSERVATION}'")
        ),
        kg,
        "the ordinary fts table is written by the move itself, since no trigger \
         maintains it"
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM fts_notes_rowids WHERE namespace = '{kg}'")
        ),
        2,
        "and so is its rowid sidecar"
    );
    assert_eq!(
        text(
            &conn,
            &format!("SELECT namespace FROM knowledge_sections WHERE id = '{SECTION}'")
        ),
        built.spec.knowledge,
        "a section follows its atom rather than being routed on its own"
    );
}

/// The collision a consolidation actually hits, and the one whose refusal is
/// weakest.
///
/// `idx_notes_namespace_kind_key` is PARTIAL, and `collisions_for` admits only
/// constraints it can enumerate exactly, so this clash is not in the pre-flight
/// list. The move is still refused — by the constraint itself, mid-statement —
/// which is what the module means by deciding the quality of a refusal rather
/// than whether one happens. The arm asserts both halves: it refuses, and it
/// refuses WITHOUT naming the rows. What a caller actually receives is thinner
/// than the index name — SQLite reports the index's COLUMNS — so there is not
/// even a name to look up, which is the gap a caller reporting to a human has
/// to close somewhere.
///
/// If the pre-flight ever learns partial predicates, this arm goes red and is
/// upgraded to the named form, which is the point of writing it this way round.
#[test]
fn a_note_key_already_taken_refuses_the_move_without_naming_the_rows() {
    let (conn, built) = fixture(FixtureSpec {
        collisions: true,
        stale_fts: false,
        ..FixtureSpec::movable("tenant:tnt_fixture")
    });
    // Every other planted clash comes out, because each of them IS in the
    // pre-flight list and would refuse before the note's constraint ever ran.
    // An arm about the refusal a partial index does NOT produce has to be the
    // only refusal on the path, or it is an arm about something else.
    conn.execute(
        "DELETE FROM knowledge_atoms WHERE id = ?1",
        [built.colliding_atom.expect("planted")],
    )
    .expect("remove the slug clash");
    conn.execute(
        "DELETE FROM graph_edges WHERE id = ?1",
        [built.colliding_edge_triple.expect("planted")],
    )
    .expect("remove the triple clash");
    let source = built.spec.source.clone();
    let before = count(
        &conn,
        &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{source}'"),
    );

    match attempt(&conn, &request(&built)) {
        Err(MoveError::Sqlite(error)) => {
            let message = error.to_string();
            // SQLite names the COLUMNS of the index it refused on, not the
            // index, so the caller gets even less than the index name: a
            // column list, no rows, and nothing that says which two records
            // clashed. That is the whole point of the arm, and reading it out
            // of the error rather than assuming the index name is what makes
            // the point true rather than nearly true.
            assert!(
                message.contains("notes.namespace, notes.kind, notes.key"),
                "the constraint raised it, so its column list is all the caller \
                 gets: {message}"
            );
            assert!(
                !message.contains("idx_notes_namespace_kind_key"),
                "and not even the index name, which a caller could at least \
                 have looked up: {message}"
            );
        }
        Err(MoveError::Collisions { collisions }) => panic!(
            "the pre-flight has learned partial predicates: upgrade this arm to \
             assert the named collision. {collisions:?}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{source}'")
        ),
        before,
        "nothing moved: the enclosing transaction is what makes a mid-statement \
         refusal atomic, and the move opens none of its own"
    );
}

/// A slug already taken under the target, named by the pre-flight.
#[test]
fn an_atom_slug_already_taken_is_named_before_anything_is_written() {
    let (conn, built) = fixture(FixtureSpec {
        collisions: true,
        stale_fts: false,
        ..FixtureSpec::movable("tenant:tnt_fixture")
    });
    // Drop the note-key clash so the atom is the only thing in the way; the
    // note constraint is partial and would refuse first, from a different
    // layer, and an arm that cannot say which refusal it saw is not an arm.
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        [built.colliding_note.expect("planted")],
    )
    .expect("remove the note-key clash");
    // The edge triple is planted by the same flag and IS in the pre-flight
    // list, so leaving it would make this arm assert about a list of two.
    conn.execute(
        "DELETE FROM graph_edges WHERE id = ?1",
        [built.colliding_edge_triple.expect("planted")],
    )
    .expect("remove the triple clash");

    let found = collisions(attempt(&conn, &request(&built)));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].table, "knowledge_atoms");
    assert_eq!(found[0].constraint, "idx_knowledge_atoms_ns_slug");
    assert_eq!(found[0].target, built.spec.knowledge);
    assert_eq!(
        found[0].key, "shared-slug",
        "the key is rendered in the constraint's own column order, and it \
         identifies both rows: the one holding it in the target and the one \
         that would have carried it there"
    );
}

/// The edge triple, which is the constraint that actually refuses a graph edge
/// move.
#[test]
fn an_edge_triple_already_taken_is_named_before_anything_is_written() {
    let (conn, built) = fixture(FixtureSpec {
        collisions: true,
        stale_fts: false,
        ..FixtureSpec::movable("tenant:tnt_fixture")
    });
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        [built.colliding_note.expect("planted")],
    )
    .expect("remove the note-key clash");
    conn.execute(
        "DELETE FROM knowledge_atoms WHERE id = ?1",
        [built.colliding_atom.expect("planted")],
    )
    .expect("remove the slug clash");

    let found = collisions(attempt(&conn, &request(&built)));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].table, "graph_edges");
    assert_eq!(found[0].constraint, "idx_graph_edges_unique_triple");
    assert_eq!(
        found[0].key,
        format!("{ENTITY}, {NOTE_OBSERVATION}, annotates"),
        "source, target and relation in index order"
    );
}

/// The pre-flight asks each constraint against every route's target, and the
/// question it asks is about the whole table rather than about the rows that
/// route would move. So a clash sitting under a namespace this class never
/// travels to refuses the move anyway.
///
/// `collisions_for` joins `knowledge_atoms` source-side against the target with
/// no class predicate, while the mover carries atoms to one namespace and
/// nothing else. Plant the source atom's slug under `kg` — where an atom can
/// never arrive, because no route sends one there — and the move is refused
/// over a row nothing would ever have written across.
///
/// Today's behaviour errs toward refusing a LEGAL move, which is the safe
/// direction and is visible when it happens. The arm pins that direction rather
/// than the outcome anyone wants, and says so in its own failure message: the
/// day the pre-flight narrows its population per route, this goes red and gets
/// upgraded to assert the move SUCCEEDS. Written this way round so nobody has
/// to remember the follow-up exists.
#[test]
fn a_clash_under_a_namespace_the_class_never_routes_to_still_refuses_the_move() {
    let (conn, built) = fixture(FixtureSpec::movable("tenant:tnt_fixture"));
    let kg = built.spec.kg.clone();
    assert!(
        routes(&built.spec)
            .iter()
            .all(|(class, target)| *class != "atom" || *target != kg),
        "the arm means nothing unless no atom route targets kg: {:?}",
        routes(&built.spec)
    );

    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        rusqlite::params![
            ATOM_SLUG_HOLDER,
            &kg,
            "shared-slug",
            "unreachable by any atom route",
            1_700_000_000_i64
        ],
    )
    .expect("plant a slug under a namespace atoms never reach");

    let found = collisions(attempt(&conn, &request(&built)));
    assert_eq!(
        found.len(),
        1,
        "the pre-flight has learned per-route populations: upgrade this arm to \
         assert the move SUCCEEDS, because this clash was never reachable. \
         {found:?}"
    );
    assert_eq!(found[0].table, "knowledge_atoms");
    assert_eq!(
        found[0].target, kg,
        "and the target it names is the one no atom was going to: that is the \
         whole of the over-refusal, readable off the refusal itself"
    );
}

/// The census reports `graph_edges`' composite primary key as an ordinary
/// per-namespace key. It can never be the constraint that refuses a move, and
/// the reason is a constraint the census does not report at all.
///
/// `sql/014-graph-edges-id-unique.sql` makes `id` unique across every namespace,
/// so two rows can never share one, so `(namespace, id)` can never clash. A
/// collision check built from the census alone therefore carries a branch that
/// cannot fire and a message no caller will ever be shown. The arm is here
/// because nothing in the enumerated set says the branch is dead.
#[test]
fn the_edge_primary_key_can_never_be_the_constraint_that_refuses() {
    let (conn, built) = fixture(FixtureSpec::movable("tenant:tnt_fixture"));
    let kg = built.spec.kg.clone();

    let refused = conn
        .execute(
            "INSERT INTO graph_edges \
             (id, namespace, source_id, target_id, relation, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'depends_on', 1, 1)",
            rusqlite::params![EDGE, &kg, ENTITY, NOTE_TASK],
        )
        .expect_err("a second row carrying this id must be refused");
    // SQLite names the COLUMN a unique index is on, never the index, so this
    // asserts what it actually says. The index behind it is named from the
    // schema below rather than read out of an error string.
    assert!(
        refused.to_string().contains("graph_edges.id"),
        "the global index refuses first, which is why the composite key never \
         gets the chance: {refused}"
    );
    let global: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_graph_edges_id_unique'",
            [],
            |row| row.get(0),
        )
        .expect("read the index out of the schema");
    assert_eq!(
        global, 1,
        "the refusal above comes from this index; naming it from the schema is \
         what ties the observed refusal to the declaration that causes it"
    );

    let by_index: BTreeSet<String> = census(&conn)
        .expect("census")
        .constraints_on("graph_edges")
        .iter()
        .map(|c| c.index.clone())
        .collect();
    assert!(
        !by_index.contains("idx_graph_edges_id_unique"),
        "and it is absent from the census, because it does not name namespace — \
         a constraint the census never reports is what bounds the reachability \
         of one it does"
    );
}

/// The partial predicate, asserted in the direction that over-refuses.
///
/// The soft-deleted note carries the same `(kind, key)` as a live note already
/// under the target, and `idx_notes_namespace_kind_key` is partial on
/// `deleted_at IS NULL`, so SQLite permits it. A collision check that reads an
/// index's key columns and ignores its `WHERE` would refuse this move, and the
/// refusal would be invisible to any arm that only plants live rows.
#[test]
fn a_soft_deleted_note_sharing_a_live_key_does_not_block_the_move() {
    let (conn, built) = fixture(FixtureSpec {
        collisions: true,
        stale_fts: false,
        ..FixtureSpec::movable("tenant:tnt_fixture")
    });
    // Leave the target's holder in place but make the LIVE source note stop
    // clashing with it, so the only row still sharing that key is the deleted
    // one.
    conn.execute(
        "UPDATE notes SET key = 'moved-on' WHERE id = ?1",
        [NOTE_OBSERVATION],
    )
    .expect("re-key the live note");
    conn.execute(
        "DELETE FROM knowledge_atoms WHERE id = ?1",
        [built.colliding_atom.expect("planted")],
    )
    .expect("remove the slug clash");
    conn.execute(
        "DELETE FROM graph_edges WHERE id = ?1",
        [built.colliding_edge_triple.expect("planted")],
    )
    .expect("remove the triple clash");

    let counts = attempt(&conn, &request(&built)).expect("a deleted row occupies no key");
    assert_eq!(counts.subjects.get("note:observation").copied(), Some(2));
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT COUNT(*) FROM notes WHERE id = '{NOTE_DELETED}' AND namespace = '{}'",
                built.spec.kg
            )
        ),
        1
    );
}

/// The collision the base tables cannot show.
///
/// `fts_notes_rowids` is keyed `(namespace, subject_id)` and keeps rows whose
/// base record is gone — `sql/024-fts-rowid-map.sql` preserves them on purpose,
/// and `stores::text::delete_document_statement` documents the crash that
/// produces one. Nothing in `notes` shows a conflict, so a pre-flight reading
/// only the subject tables reports this move as clean.
#[test]
fn a_stale_index_row_refuses_a_move_the_subject_tables_permit() {
    let (conn, built) = fixture(FixtureSpec {
        stale_fts: true,
        ..FixtureSpec::movable("tenant:tnt_fixture")
    });
    let kg = built.spec.kg.clone();
    assert_eq!(built.stale_fts_subject, Some(NOTE_OBSERVATION));
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{kg}'")
        ),
        0,
        "nothing in the subject table shows a conflict, which is the point"
    );

    let outcome = attempt(&conn, &request(&built));
    let named = match outcome {
        Ok(counts) => panic!(
            "the shadow row did not stop the move; the index sidecar now holds \
             two rows for one key or one of them was overwritten: {counts:?}"
        ),
        Err(MoveError::Collisions { collisions }) => {
            assert_eq!(collisions.len(), 1, "{collisions:?}");
            assert_eq!(collisions[0].table, "fts_notes_rowids");
            true
        }
        Err(MoveError::Sqlite(error)) => {
            assert!(
                error.to_string().contains("UNIQUE constraint failed"),
                "{error}"
            );
            false
        }
        Err(other) => panic!("expected a collision refusal, got {other}"),
    };
    let source = built.spec.source.clone();
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{source}'")
        ),
        3,
        "named={named}: either way nothing moved, because the caller's \
         transaction wraps the whole attempt"
    );
}

/// An fts5 table over external content accepts an `UPDATE` of `namespace`,
/// reports success, and changes nothing. It changes when its base row is
/// written.
///
/// Stated the other way: a mover that writes this pair directly passes every rc
/// check and asserts nothing. The failure it hides is silent, so the assertion
/// has to be on the VALUE after the write, never on the write's return.
#[test]
fn the_knowledge_pair_ignores_a_direct_write_and_follows_its_base_row() {
    let (conn, built) = fixture(FixtureSpec::movable("tenant:tnt_fixture"));
    let target = built.spec.knowledge.clone();
    let source = built.spec.source.clone();

    conn.execute(
        &format!("UPDATE fts_knowledge SET namespace = '{target}' WHERE id = '{ATOM}'"),
        [],
    )
    .expect("the write is ACCEPTED, which is exactly the hazard");
    assert_eq!(
        text(
            &conn,
            &format!("SELECT namespace FROM fts_knowledge WHERE id = '{ATOM}'")
        ),
        source,
        "an external-content fts5 table reads through to its content object, so \
         the accepted write changed nothing"
    );

    attempt(&conn, &request(&built)).expect("move the store");

    assert_eq!(
        text(
            &conn,
            &format!("SELECT namespace FROM fts_knowledge WHERE id = '{ATOM}'")
        ),
        target,
        "writing the atom is what moves the index entry"
    );
    assert_eq!(
        text(
            &conn,
            &format!("SELECT namespace FROM fts_sections WHERE id = '{SECTION}'")
        ),
        target,
        "and fts_sections_au must still name namespace in its UPDATE OF list: \
         V2 narrowed that trigger once already, and a second narrowing that \
         dropped the column would stop indexing sections with no error"
    );
}

/// The split the fixture and the mover both depend on, asserted against the
/// live schema rather than against a reading of `sql/`.
///
/// Both write `fts_notes` and `fts_entities` by hand and leave the knowledge
/// pair to triggers. A migration adding a trigger over the first pair would
/// double-index every row either of them writes.
#[test]
fn the_ordinary_fts_tables_have_no_triggers_and_the_knowledge_pair_has_them() {
    let conn = migrated();
    let mut stmt = conn
        .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master WHERE type = 'trigger'")
        .expect("read the triggers");
    let triggers: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert!(
        !triggers.is_empty(),
        "an empty trigger list would make both halves of this arm pass without \
         reading anything"
    );

    for (name, sql) in &triggers {
        for table in ["fts_notes", "fts_entities"] {
            assert!(
                !sql.contains(table),
                "{name} writes {table}, which the move writes itself"
            );
        }
    }
    for table in ["fts_knowledge", "fts_sections"] {
        assert!(
            triggers.iter().any(|(_, sql)| sql.contains(table)),
            "{table} is maintained by trigger; if that stopped being true the \
             move would have to write it, which an external-content table \
             cannot accept"
        );
    }
}

/// The half of the census no source-parsing instrument can reach: a table
/// created at run time, named for the embedding model, present in no `.sql`
/// file. Without a vector row in the store, the census and a declaration scan
/// answer identically and cannot be told apart.
#[cfg(feature = "vectors")]
#[test]
fn a_runtime_created_vector_table_is_in_the_census_and_in_no_declaration() {
    use crate::namespace_move_fixture::add_vector_row;

    crate::extension::ensure_extensions_loaded();
    let (conn, built) = fixture(FixtureSpec::movable("tenant:tnt_fixture"));
    assert!(
        census(&conn).expect("census").vector_tables().is_empty(),
        "a store that has never embedded has no vector table"
    );

    add_vector_row(&conn, "fixture_model", &built.spec.source, NOTE_OBSERVATION)
        .expect("create the vector table and write one row");

    let after = census(&conn).expect("census");
    assert_eq!(after.vector_tables(), vec!["vec_fixture_model"]);
    assert!(
        after.unenumerable.iter().any(|t| t == "vec_fixture_model"),
        "vec0 declares its own PRIMARY KEY and reports no index list, so the \
         table is recorded as unread rather than as constraint-free"
    );
}

/// The fixture's own shape, asserted once so every arm above reads a store it
/// can trust rather than re-counting it.
#[test]
fn the_fixture_plants_what_it_says_it_plants() {
    let (conn, built) = fixture(FixtureSpec::new("tenant:tnt_fixture"));
    let source = built.spec.source.clone();

    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes WHERE namespace = '{source}'")
        ),
        3
    );
    for (table, expected) in [
        ("fts_notes", 4),
        ("fts_notes_rowids", 5),
        ("fts_entities", 1),
        ("fts_entities_rowids", 1),
        ("graph_edges", 2),
        ("knowledge_atoms", 2),
        ("knowledge_sections", 1),
    ] {
        assert_eq!(
            count(&conn, &format!("SELECT COUNT(*) FROM {table}")),
            expected,
            "{table}"
        );
    }
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM notes_seq WHERE note_id = '{NOTE_OBSERVATION}'")
        ),
        1,
        "the list-cursor ledger row comes from a trigger, so a statement-level \
         insert leaves the same state a pack write would"
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM fts_knowledge WHERE id = '{ATOM}'")
        ),
        1,
        "the knowledge index follows its base row by trigger and the builder \
         never writes it"
    );
}
