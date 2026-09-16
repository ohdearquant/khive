//! What a live store's schema says about `namespace` (ADR-189).
//!
//! Two questions this answers, both of which used to be answered by reading the
//! schema sources by hand:
//!
//! 1. which tables carry a `namespace` column, and
//! 2. which uniqueness constraints that column participates in.
//!
//! Neither is answerable from the sources. The `vec_{model_key}` vector tables
//! are created at runtime, one per embedding model (`backend.rs`), appear in no
//! `.sql` file and in no migration, and their membership is a property of a
//! given store rather than of the code.
//!
//! The constraint half is the one that bites. Two independent hand enumerations
//! of the namespace-bearing uniqueness constraints agreed on nine; there are
//! fourteen. One asked which `CREATE UNIQUE INDEX` bodies name `namespace` and
//! could not see a composite `PRIMARY KEY`; the other asked which composite
//! primary keys name it and could not see an index; neither could see an index
//! whose third key is an expression (`json_extract(properties, '$.external_id')`
//! on `notes`) rather than a column name. Every miss is a shape the question
//! could not express, which is why the answer here is a derivation and not a
//! longer list.
//!
//! `PRAGMA index_list` reports primary keys (`origin = "pk"`), `UNIQUE` table
//! constraints (`"u"`) and `CREATE UNIQUE INDEX` (`"c"`) through one surface,
//! and `PRAGMA index_xinfo` names the `namespace` column inside an expression
//! index like any other. So a constraint a future migration adds is in the
//! census on the next run, with no edit here.

use std::collections::BTreeSet;

use rusqlite::Connection;

/// Where a uniqueness constraint came from, as SQLite reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConstraintOrigin {
    /// `PRIMARY KEY` on the table (`PRAGMA index_list` origin `pk`).
    PrimaryKey,
    /// A `UNIQUE` constraint in the table body (origin `u`).
    UniqueConstraint,
    /// A standalone `CREATE UNIQUE INDEX` (origin `c`).
    UniqueIndex,
}

impl ConstraintOrigin {
    fn parse(origin: &str) -> Option<Self> {
        match origin {
            "pk" => Some(Self::PrimaryKey),
            "u" => Some(Self::UniqueConstraint),
            "c" => Some(Self::UniqueIndex),
            _ => None,
        }
    }
}

/// A table carrying a `namespace` column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NamespaceTable {
    pub name: String,
    /// `CREATE VIRTUAL TABLE` — fts5 and vec0. These accept no `UPDATE` of an
    /// indexed column and expose no index list, so they are moved by delete and
    /// re-insert and contribute no constraints.
    pub virtual_table: bool,
}

/// One uniqueness constraint in which `namespace` participates.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NamespaceConstraint {
    pub table: String,
    /// The index backing it. Implicit primary-key indexes are named by SQLite
    /// (`sqlite_autoindex_<table>_<n>`).
    pub index: String,
    pub origin: ConstraintOrigin,
    /// Key columns in index order. `None` is an expression, which has no column
    /// name — the shape both hand enumerations were blind to.
    pub columns: Vec<Option<String>>,
    /// A partial index (`... WHERE ...`). Its constraint binds only the rows its
    /// predicate admits, so a collision check that ignores this refuses moves
    /// SQLite would have accepted.
    pub partial: bool,
}

impl NamespaceConstraint {
    /// True when every key column is a plain column name, so a caller can build
    /// a collision query from this constraint alone.
    pub fn columns_are_nameable(&self) -> bool {
        self.columns.iter().all(Option::is_some)
    }
}

/// The census of one live store.
///
/// `unenumerable` is not decoration. A virtual table answers `PRAGMA index_list`
/// with nothing or with an error depending on the module, and an empty answer
/// there is indistinguishable from "no constraints" — so the tables whose
/// constraints were never readable are listed by name instead of being folded
/// into the clean case.
#[derive(Debug, Clone, Default)]
pub struct NamespaceCensus {
    /// The file this census describes, from `PRAGMA database_list`, empty for
    /// an in-memory database.
    ///
    /// A census is per connection and a pack can be assigned its own backend,
    /// so one store's records can live in three SQLite files. A report that
    /// does not say which file it read is unusable the moment there is a second
    /// one.
    pub database: String,
    pub tables: Vec<NamespaceTable>,
    pub constraints: Vec<NamespaceConstraint>,
    pub unenumerable: Vec<String>,
}

impl NamespaceCensus {
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }

    /// The vector tables, which exist only in a store that has embedded with a
    /// model. Named by prefix because their suffix is the model key.
    pub fn vector_tables(&self) -> Vec<&str> {
        self.tables
            .iter()
            .filter(|t| t.name.starts_with("vec_"))
            .map(|t| t.name.as_str())
            .collect()
    }

    pub fn constraints_on(&self, table: &str) -> Vec<&NamespaceConstraint> {
        self.constraints
            .iter()
            .filter(|c| c.table == table)
            .collect()
    }
}

/// Double an identifier's embedded quotes so it can be interpolated into a
/// `PRAGMA`. Pragmas take no bound parameters for their argument, so the name
/// has to be inlined; every name here comes from `sqlite_master`, and quoting it
/// anyway keeps that a property of this function rather than of its callers.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Every table in the main schema carrying a `namespace` column.
pub fn namespace_tables(conn: &Connection) -> rusqlite::Result<Vec<NamespaceTable>> {
    let mut stmt = conn.prepare(
        "SELECT name, COALESCE(sql, '') FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let candidates: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut tables = Vec::new();
    for (name, sql) in candidates {
        if !table_has_namespace_column(conn, &name)? {
            continue;
        }
        // Tokenized rather than sliced: the stored text is whatever the
        // migration wrote, and a length-prefix comparison is off by one the
        // moment anyone reformats it.
        let mut head = sql.split_whitespace();
        let virtual_table = matches!(
            (head.next(), head.next(), head.next()),
            (Some(create), Some(virt), Some(table))
                if create.eq_ignore_ascii_case("CREATE")
                    && virt.eq_ignore_ascii_case("VIRTUAL")
                    && table.eq_ignore_ascii_case("TABLE")
        );
        tables.push(NamespaceTable {
            name,
            virtual_table,
        });
    }
    Ok(tables)
}

fn table_has_namespace_column(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let column: String = row.get(1)?;
        if column.eq_ignore_ascii_case("namespace") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The full census: namespace-carrying tables, and every uniqueness constraint
/// `namespace` participates in.
pub fn census(conn: &Connection) -> rusqlite::Result<NamespaceCensus> {
    let tables = namespace_tables(conn)?;
    let mut constraints = Vec::new();
    let mut unenumerable = Vec::new();

    for table in &tables {
        match unique_constraints_naming_namespace(conn, &table.name) {
            // A virtual table that reports NO indexes has not said it has none:
            // `PRAGMA index_list` describes the indexes SQLite keeps for a
            // table, and a module keeps its own. vec0 declares
            // `subject_id TEXT PRIMARY KEY` in its own DDL and reports nothing
            // here. So an empty answer from a virtual table is recorded as
            // unread rather than as clean; a module that does report rows is
            // read like any other table.
            Ok(found) if found.is_empty() && table.virtual_table => {
                unenumerable.push(table.name.clone())
            }
            Ok(found) => constraints.extend(found),
            Err(_) if table.virtual_table => unenumerable.push(table.name.clone()),
            Err(error) => return Err(error),
        }
    }
    constraints.sort();
    Ok(NamespaceCensus {
        database: main_database_file(conn)?,
        tables,
        constraints,
        unenumerable,
    })
}

/// The file backing `main`, or an empty string for an in-memory database.
fn main_database_file(conn: &Connection) -> rusqlite::Result<String> {
    let mut stmt = conn.prepare("PRAGMA database_list")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "main" {
            return Ok(row.get::<_, Option<String>>(2)?.unwrap_or_default());
        }
    }
    Ok(String::new())
}

fn unique_constraints_naming_namespace(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Vec<NamespaceConstraint>> {
    let list_sql = format!("PRAGMA index_list({})", quote_ident(table));
    let mut list = conn.prepare(&list_sql)?;
    let indexes: Vec<(String, i64, String, i64)> = list
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut found = Vec::new();
    for (index, unique, origin, partial) in indexes {
        if unique == 0 {
            continue;
        }
        let Some(origin) = ConstraintOrigin::parse(&origin) else {
            continue;
        };
        let columns = index_key_columns(conn, &index)?;
        let named = columns.iter().any(|c| {
            c.as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("namespace"))
        });
        // An expression key column is reported as a null name, so an index
        // over `lower(namespace)` names nothing here at all. Measured:
        // `CREATE UNIQUE INDEX i ON t(lower(namespace), id)` gives xinfo rows
        // `-2|NULL|key=1` and `0|id|key=1`. The comm external-id index survives
        // the column read only because its namespace is a literal first column
        // and the expression sits on a different one.
        //
        // So an index carrying an expression is additionally read from its own
        // DDL. The two instruments cover each other exactly: only an autoindex
        // has a null `sql`, and an autoindex is a table constraint over a
        // column list, which cannot carry an expression.
        let named = named
            || (columns.iter().any(Option::is_none) && index_ddl_names_namespace(conn, &index)?);
        if !named {
            continue;
        }
        found.push(NamespaceConstraint {
            table: table.to_string(),
            index,
            origin,
            columns,
            partial: partial != 0,
        });
    }
    Ok(found)
}

/// Whether an index's own `CREATE INDEX` text mentions `namespace` as a word.
///
/// Only consulted for an index that has an expression key column, where the
/// column read cannot answer. A text match is coarser than a parse, and it errs
/// in the safe direction on purpose: a false positive puts one more constraint
/// in the refusal set, which refuses a move SQLite would have allowed and is
/// visible to whoever reads the refusal. A miss would corrupt rows silently.
fn index_ddl_names_namespace(conn: &Connection, index: &str) -> rusqlite::Result<bool> {
    let ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [index],
            |row| row.get(0),
        )
        .unwrap_or(None);
    Ok(ddl.is_some_and(|ddl| mentions_namespace_as_a_word(&ddl)))
}

/// `namespace` bounded by non-identifier characters, so `namespace_hash` and
/// `ns_namespace` do not match.
fn mentions_namespace_as_a_word(text: &str) -> bool {
    const NEEDLE: &str = "namespace";
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower[from..].find(NEEDLE) {
        let start = from + offset;
        let end = start + NEEDLE.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The key columns of an index, in index order, `None` for an expression.
///
/// `index_xinfo` rather than `index_info`: the former marks which entries are
/// key columns (`key = 1`) versus the auxiliary columns SQLite appends, and
/// reports expressions with `cid = -2` and a null name instead of omitting them.
fn index_key_columns(conn: &Connection, index: &str) -> rusqlite::Result<Vec<Option<String>>> {
    let sql = format!("PRAGMA index_xinfo({})", quote_ident(index));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, Option<String>>(2)?, row.get::<_, i64>(5)?))
    })?;
    let mut columns = Vec::new();
    for row in rows {
        let (name, key) = row?;
        if key == 1 {
            columns.push(name);
        }
    }
    Ok(columns)
}

/// The tables a move must not write, whatever the census finds.
///
/// `events` is the record of what happened under the namespace it happened
/// under. The two `ann_consumer_*` tables are excluded by the write-log
/// decision: the log is appended to at a fresh `seq` and no watermark is edited,
/// so moving a watermark would claim a consumer is caught up on entries it has
/// never seen.
pub const TABLES_EXCLUDED_FROM_MOVE: &[&str] =
    &["events", "ann_consumer_watermark", "ann_consumer_pending"];

/// `note_streams` is read by a move and never written by one. Four triggers in
/// the stream schema abort an `UPDATE` of a member note's namespace, the delete,
/// and every write to the ledger rows, so a move reaching a stream member
/// refuses from a pre-flight read. Its `(namespace, stream, seq)` primary key is
/// therefore in the census and out of reach, and it is kept apart from
/// [`TABLES_EXCLUDED_FROM_MOVE`] because the reasons differ: those tables are a
/// decision about what a move should carry, this one is what the schema allows.
pub const TABLE_REFUSED_BY_SCHEMA: &str = "note_streams";

/// The constraints a move can actually violate: every one the census finds,
/// minus those on tables the move never writes.
pub fn reachable_constraints(census: &NamespaceCensus) -> Vec<&NamespaceConstraint> {
    let excluded: BTreeSet<&str> = TABLES_EXCLUDED_FROM_MOVE
        .iter()
        .copied()
        .chain(std::iter::once(TABLE_REFUSED_BY_SCHEMA))
        .collect();
    census
        .constraints
        .iter()
        .filter(|c| !excluded.contains(c.table.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::run_migrations;

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory connection");
        run_migrations(&mut conn).expect("migrate to the current schema");
        conn
    }

    fn constraint_names(census: &NamespaceCensus) -> BTreeSet<(String, String)> {
        census
            .constraints
            .iter()
            .map(|c| (c.table.clone(), c.index.clone()))
            .collect()
    }

    /// The whole point of the module, stated as the list it replaces. Every row
    /// here was verified at source; five of them are the ones two hand
    /// enumerations missed, and they are called out so a future edit that drops
    /// one has to argue with the reason rather than with a name.
    #[test]
    fn census_finds_every_namespace_bearing_uniqueness_constraint() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        let found = constraint_names(&report);

        let by_index: BTreeSet<&str> = found.iter().map(|(_, i)| i.as_str()).collect();
        for expected in [
            "idx_notes_namespace_kind_key",
            "idx_comm_message_external_id",
            "idx_graph_edges_unique_triple",
            "idx_knowledge_atoms_ns_slug",
            "idx_knowledge_domains_ns_slug",
            "idx_brain_serve_ledger_unique",
        ] {
            assert!(
                by_index.contains(expected),
                "census missed the unique index {expected}; found {by_index:?}"
            );
        }

        let by_table: BTreeSet<&str> = found.iter().map(|(t, _)| t.as_str()).collect();
        for expected in [
            "graph_edges",
            "brain_implicit_mass",
            "brain_profile_snapshots",
            "ann_consumer_watermark",
            "ann_consumer_pending",
            "note_streams",
            "fts_notes_rowids",
            "fts_entities_rowids",
        ] {
            assert!(
                by_table.contains(expected),
                "census missed a namespace-bearing primary key on {expected}; found {by_table:?}"
            );
        }
    }

    /// An expression index has no column name for its third key. This is the
    /// shape that defeated both hand enumerations, so it gets its own assertion
    /// rather than riding on the count above.
    #[test]
    fn an_expression_index_is_found_and_reports_its_expression_as_unnameable() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        let external_id = report
            .constraints
            .iter()
            .find(|c| c.index == "idx_comm_message_external_id")
            .expect("the comm external-id index is a namespace-bearing unique index");

        assert_eq!(external_id.table, "notes");
        assert_eq!(external_id.origin, ConstraintOrigin::UniqueIndex);
        assert!(
            external_id.partial,
            "the index is filtered, and a collision check ignoring that refuses moves SQLite accepts"
        );
        assert!(
            !external_id.columns_are_nameable(),
            "the third key is json_extract(...), which has no column name: {:?}",
            external_id.columns
        );
        assert!(
            external_id
                .columns
                .iter()
                .any(|c| c.as_deref() == Some("namespace")),
            "namespace is still named inside an expression index: {:?}",
            external_id.columns
        );
    }

    /// The gap the column read alone cannot close: an index over
    /// `lower(namespace)` names nothing in `index_xinfo`, because an expression
    /// key column is reported with a null name. The comm external-id index does
    /// not exercise this - its namespace is a literal column and the expression
    /// is a different one - so the arm builds the shape the schema does not yet
    /// have.
    #[test]
    fn an_index_whose_namespace_is_inside_an_expression_is_found_from_its_own_ddl() {
        let conn = migrated();
        conn.execute_batch(
            "CREATE UNIQUE INDEX idx_expr_ns ON notes(lower(namespace), kind, name)",
        )
        .expect("an index whose namespace sits inside an expression");

        // Control on the instrument, not on the answer: the column read alone
        // must NOT see it, or this arm is proving nothing about the DDL path.
        let columns = index_key_columns(&conn, "idx_expr_ns").expect("xinfo");
        assert!(
            !columns.iter().any(|c| c.as_deref() == Some("namespace")),
            "control: index_xinfo must not name namespace here, got {columns:?}"
        );

        let report = super::census(&conn).expect("census");
        assert!(
            report.constraints.iter().any(|c| c.index == "idx_expr_ns"),
            "an index carrying an expression is read from its own DDL"
        );
    }

    /// The DDL read is a text match, so it is bounded to whole words. A column
    /// merely spelled like the one we care about must not enter the refusal set.
    #[test]
    fn a_namespace_shaped_column_name_does_not_match_the_ddl_read() {
        assert!(mentions_namespace_as_a_word(
            "CREATE UNIQUE INDEX i ON t(lower(namespace), id)"
        ));
        assert!(mentions_namespace_as_a_word("ON t(NAMESPACE)"));
        assert!(!mentions_namespace_as_a_word(
            "CREATE UNIQUE INDEX i ON t(lower(namespace_hash), id)"
        ));
        assert!(!mentions_namespace_as_a_word("ON t(ns_namespace_key)"));
    }

    /// A composite primary key reports through the same surface as an index,
    /// which is the half the index-only enumeration could not see.
    #[test]
    fn a_composite_primary_key_reports_as_a_constraint_with_its_columns() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        let edges = report
            .constraints_on("graph_edges")
            .into_iter()
            .find(|c| c.origin == ConstraintOrigin::PrimaryKey)
            .expect("graph_edges is PRIMARY KEY (namespace, id)");
        assert_eq!(
            edges.columns,
            vec![Some("namespace".to_string()), Some("id".to_string())]
        );
    }

    /// The arm that catches the failure this module exists for: a constraint
    /// arriving with a migration is in the census on the next run, with no edit
    /// to any list.
    #[test]
    fn a_constraint_added_after_this_code_was_written_is_found_with_no_code_change() {
        let conn = migrated();
        let before = constraint_names(&super::census(&conn).expect("census"));
        assert!(
            !before.iter().any(|(_, i)| i == "idx_future_ns_status"),
            "control: the index under test must not already exist"
        );

        conn.execute_batch(
            "CREATE UNIQUE INDEX idx_future_ns_status ON notes(namespace, status, name)",
        )
        .expect("a later migration adds a namespace-bearing unique index");

        let after = constraint_names(&super::census(&conn).expect("census"));
        assert!(
            after
                .iter()
                .any(|(t, i)| t == "notes" && i == "idx_future_ns_status"),
            "the census has to find a constraint nobody told it about; found {after:?}"
        );
        assert_eq!(
            after.len(),
            before.len() + 1,
            "and it must find exactly the one that was added"
        );
    }

    /// A non-unique index naming namespace is not a constraint, and counting it
    /// would refuse moves that are legal.
    #[test]
    fn a_non_unique_index_naming_namespace_is_not_a_constraint() {
        let conn = migrated();
        let before = super::census(&conn).expect("census").constraints.len();
        conn.execute_batch("CREATE INDEX idx_plain_ns_salience ON notes(namespace, salience)")
            .expect("plain index");
        let after = super::census(&conn).expect("census").constraints.len();
        assert_eq!(
            after, before,
            "a non-unique index is not a uniqueness constraint"
        );
    }

    /// The tables a move never writes carry constraints that cannot collide, and
    /// the reachable set is the census minus exactly those.
    #[test]
    fn the_reachable_set_excludes_the_tables_a_move_never_writes() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        let reachable: BTreeSet<&str> = reachable_constraints(&report)
            .into_iter()
            .map(|c| c.table.as_str())
            .collect();

        for out_of_reach in [
            "note_streams",
            "ann_consumer_watermark",
            "ann_consumer_pending",
        ] {
            assert!(
                report.constraints.iter().any(|c| c.table == out_of_reach),
                "control: {out_of_reach} must be IN the census, or this arm proves nothing"
            );
            assert!(
                !reachable.contains(out_of_reach),
                "{out_of_reach} is out of reach for a move"
            );
        }
        for in_reach in [
            "notes",
            "graph_edges",
            "fts_notes_rowids",
            "brain_serve_ledger",
        ] {
            assert!(reachable.contains(in_reach), "{in_reach} is reachable");
        }
    }

    /// A census is per connection, and a store can be three files. The report
    /// says which one it read.
    #[test]
    fn a_census_names_the_database_it_read() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        assert_eq!(
            report.database, "",
            "an in-memory database has no file, and the empty string is that answer"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("second-backend.db");
        let mut file_conn = Connection::open(&path).expect("open a file-backed store");
        run_migrations(&mut file_conn).expect("migrate the second backend");
        let file_census = super::census(&file_conn).expect("census");
        // SQLite reports the path it resolved, and macOS hands a temp dir out
        // through a symlink, so the comparison is between two resolved paths.
        let resolved = std::fs::canonicalize(&path).expect("resolve the store path");
        let reported = std::fs::canonicalize(&file_census.database)
            .expect("the census names a path that exists");
        assert_eq!(reported, resolved, "a file-backed census names its file");
    }

    /// An fts5 table reports no indexes, and that is not the same statement as
    /// "has no constraints". The census says so by name.
    #[test]
    fn a_virtual_table_reporting_no_indexes_is_recorded_as_unread_not_as_clean() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        assert!(
            report.unenumerable.iter().any(|t| t == "fts_notes"),
            "fts_notes reports no index list, so its constraints are unread: {:?}",
            report.unenumerable
        );
        assert!(
            !report.unenumerable.iter().any(|t| t == "notes"),
            "control: an ordinary table's constraints ARE readable, so it is not listed"
        );
    }

    /// The table half. `notes` is the obvious one; the assertion that matters is
    /// that the fts5 virtual tables are found and marked, because a virtual
    /// table is where a namespace write behaves differently from everywhere
    /// else.
    #[test]
    fn the_table_census_finds_the_virtual_tables_and_marks_them() {
        let conn = migrated();
        let report = super::census(&conn).expect("census");
        let names = report.table_names();
        for expected in [
            "notes",
            "entities",
            "graph_edges",
            "knowledge_atoms",
            "events",
        ] {
            assert!(
                names.contains(&expected),
                "missing {expected} from {names:?}"
            );
        }
        // Pinned as a set on purpose, and this is the one place in the file where
        // pinning is right. A constraint arriving in a migration must be found
        // with no edit here, because nothing downstream has to understand it. A
        // namespace-bearing VIRTUAL TABLE arriving in a migration must break a
        // test, because a mover has to decide what happens to its rows and no
        // default is safe.
        let virtual_tables: Vec<&str> = report
            .tables
            .iter()
            .filter(|t| t.virtual_table)
            .map(|t| t.name.as_str())
            .collect();
        println!("namespace-bearing virtual tables: {virtual_tables:?}");
        assert_eq!(
            virtual_tables,
            ["fts_entities", "fts_knowledge", "fts_notes", "fts_sections"],
            "the namespace-bearing virtual tables of a freshly migrated store"
        );
        let notes = report
            .tables
            .iter()
            .find(|t| t.name == "notes")
            .expect("notes");
        assert!(
            !notes.virtual_table,
            "control: an ordinary table is not marked virtual"
        );
    }
}
