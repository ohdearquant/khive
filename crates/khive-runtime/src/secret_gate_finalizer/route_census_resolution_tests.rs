use super::*;

#[test]
fn external_note_sql_constant_requires_an_inventoried_reservation_check() {
    let route = *ROUTE_INVENTORY
        .iter()
        .find(|route| route.id == "curation.merge.note")
        .expect("note merge route is declared");
    let source_path = "khive-runtime/src/curation/merge_sql.rs";
    let checked = "fn merge_note_sql() {
        reject_reserved_secret_gate_property(merged_props);
        let _ = khive_db::stores::note::NOTE_UPSERT_SQL;
    }";
    let sites = scan_sources(&[(source_path.into(), checked.into())]).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].key, route.site);
    assert_eq!(sites[0].target, Substrate::Note);
    assert_eq!(sites[0].class, DetectedClass::WholeObject);
    assert!(sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"));
    assert!(check_inventory(&sites, &[route], 0).is_ok());
    assert!(check_inventory(&sites, &[], 0)
        .unwrap_err()
        .contains("unmapped khive-runtime/src/curation/merge_sql.rs::merge_note_sql"));

    let unchecked = "fn merge_note_sql() {
        let _ = khive_db::stores::note::NOTE_UPSERT_SQL;
    }";
    let sites = scan_sources(&[(source_path.into(), unchecked.into())]).unwrap();
    assert_eq!(sites.len(), 1);
    assert!(check_inventory(&sites, &[route], 0)
        .unwrap_err()
        .contains("whole-object write lacks its named check/callee"));
}

#[test]
fn aliased_note_sql_constants_require_inventory_entries() {
    assert!(!NOTE_PROPERTY_SQL_CONSTANTS.contains(&"MERGE_SQL"));
    let path = "sample/src/lib.rs";
    for (source, constant) in [
        (
            "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
             fn unlisted(conn: &Connection) { conn.prepare_cached(MERGE_SQL); }",
            "NOTE_UPSERT_SQL",
        ),
        (
            "use khive_db::stores::{note::{NOTE_UPSERT_SQL as MERGE_SQL}};
             fn unlisted(conn: &Connection) { conn.prepare_cached(MERGE_SQL); }",
            "NOTE_UPSERT_SQL",
        ),
        (
            "fn unlisted(conn: &Connection) {
                 use khive_db::stores::note::NOTE_INSERT_IF_ABSENT_SQL as MERGE_SQL;
                 conn.prepare_cached(MERGE_SQL);
             }",
            "NOTE_INSERT_IF_ABSENT_SQL",
        ),
        (
            "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
             fn unlisted(conn: &Connection) { conn.prepare_cached(self::MERGE_SQL); }",
            "NOTE_UPSERT_SQL",
        ),
        (
            "use self::MERGE_SQL as DEEP_SQL;
             use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
             fn unlisted(conn: &Connection) { conn.prepare_cached(DEEP_SQL); }",
            "NOTE_UPSERT_SQL",
        ),
    ] {
        let sites = scan_sources(&[(path.into(), source.into())]).unwrap();
        assert_eq!(sites.len(), 1, "{source}");
        assert_eq!(sites[0].key, "sample/src/lib.rs::unlisted");
        assert_eq!(sites[0].target, Substrate::Note);
        assert_eq!(sites[0].class, DetectedClass::WholeObject);
        assert!(
            sites[0]
                .evidence
                .contains(&format!("SQL constant {constant}")),
            "{source}"
        );
        assert!(
            check_inventory(&sites, &[], 0)
                .unwrap_err()
                .contains("unmapped sample/src/lib.rs::unlisted"),
            "{source}"
        );
    }

    let glob = "use khive_db::stores::note::*;
        fn unlisted(conn: &Connection) { conn.prepare_cached(NOTE_UPSERT_SQL); }";
    let sites = scan_sources(&[
        (path.into(), glob.into()),
        (
            "khive-db/src/lib.rs".into(),
            "pub mod stores { pub mod note { pub const NOTE_UPSERT_SQL: &str = \"SELECT 1\"; } }"
                .into(),
        ),
    ])
    .unwrap();
    assert_eq!(sites.len(), 1);
    assert!(check_inventory(&sites, &[], 0)
        .unwrap_err()
        .contains("unmapped sample/src/lib.rs::unlisted"));

    for qualified in ["super::MERGE_SQL", "crate::MERGE_SQL"] {
        let source = format!(
            "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
             mod nested {{ fn unlisted(conn: &Connection) {{ conn.prepare_cached({qualified}); }} }}"
        );
        let sites = scan_sources(&[(path.into(), source)]).unwrap();
        assert_eq!(sites.len(), 1, "{qualified}");
        assert_eq!(sites[0].key, "sample/src/lib.rs::nested::unlisted");
        assert!(check_inventory(&sites, &[], 0)
            .unwrap_err()
            .contains("unmapped sample/src/lib.rs::nested::unlisted"));
    }
    let reimport = "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
        mod nested {
            use super::MERGE_SQL as DEEP_SQL;
            fn unlisted(conn: &Connection) { conn.prepare_cached(DEEP_SQL); }
        }";
    let sites = scan_sources(&[(path.into(), reimport.into())]).unwrap();
    assert_eq!(sites.len(), 1);
    assert!(check_inventory(&sites, &[], 0)
        .unwrap_err()
        .contains("unmapped sample/src/lib.rs::nested::unlisted"));
}

#[test]
fn aliased_note_sql_constants_respect_lexical_shadows() {
    let path = "sample/src/lib.rs";
    for source in [
        "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
         fn reader(conn: &Connection) {
             let MERGE_SQL = \"SELECT 1\";
             conn.prepare_cached(MERGE_SQL);
         }",
        "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
         fn reader(conn: &Connection, MERGE_SQL: &str) { conn.prepare_cached(MERGE_SQL); }",
        "mod writer { use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL; }
         mod reader { fn read(conn: &Connection) { conn.prepare_cached(MERGE_SQL); } }",
        "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
         fn reader(conn: &Connection) {
             let query = |MERGE_SQL: &str| conn.prepare_cached(MERGE_SQL);
         }",
    ] {
        let sites = scan_sources(&[(path.into(), source.into())]).unwrap();
        assert!(sites.is_empty(), "{source}");
    }
    let unused = "use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;
        fn reader() {}";
    assert!(scan_sources(&[(path.into(), unused.into())])
        .unwrap()
        .is_empty());
}

fn qualified_reexport_sources(guarded: bool) -> Vec<(String, String)> {
    let writer = if guarded {
        "use crate::sql_alias::MERGE_SQL as SQL;
         fn merge_note_sql(conn: &Connection) {
             reject_reserved_secret_gate_property(merged_props);
             conn.prepare_cached(SQL);
         }"
    } else {
        "use crate::sql_alias::MERGE_SQL as SQL;
         fn merge_note_sql(conn: &Connection) { conn.prepare_cached(SQL); }"
    };
    vec![
        (
            "sample/src/lib.rs".into(),
            "mod sql_alias; mod writer;".into(),
        ),
        (
            "sample/src/sql_alias.rs".into(),
            "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;".into(),
        ),
        ("sample/src/writer.rs".into(), writer.into()),
    ]
}

fn qualified_reexport_route() -> RouteInventoryEntry {
    RouteInventoryEntry {
        site: "sample/src/writer.rs::merge_note_sql",
        ..*ROUTE_INVENTORY
            .iter()
            .find(|route| route.id == "curation.merge.note")
            .expect("note merge route is declared")
    }
}

#[test]
fn qualified_reexport_without_reservation_is_reported() {
    let sites = scan_sources(&qualified_reexport_sources(false)).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].key, "sample/src/writer.rs::merge_note_sql");
    assert_eq!(sites[0].target, Substrate::Note);
    assert_eq!(sites[0].class, DetectedClass::WholeObject);
    assert!(sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"));
    assert!(check_inventory(&sites, &[], 0)
        .unwrap_err()
        .contains("unmapped sample/src/writer.rs::merge_note_sql"));
    assert!(check_inventory(&sites, &[qualified_reexport_route()], 0)
        .unwrap_err()
        .contains("whole-object write lacks its named check/callee"));
}

#[test]
fn qualified_reexport_with_reservation_passes_inventory() {
    let sites = scan_sources(&qualified_reexport_sources(true)).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].key, "sample/src/writer.rs::merge_note_sql");
    assert_eq!(sites[0].target, Substrate::Note);
    assert_eq!(sites[0].class, DetectedClass::WholeObject);
    assert!(sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"));
    assert!(check_inventory(&sites, &[qualified_reexport_route()], 0).is_ok());
}

/// A crate whose `sql_alias` module re-exports the note upsert statement under
/// another name, a `plain` module with an unrelated constant of that same name,
/// the given writer module and any extra source files.
fn module_path_sources(writer: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut crate_root = String::from("mod sql_alias; mod plain; mod writer;");
    for (path, _) in extra {
        if let Some(name) = path
            .strip_prefix("sample/src/")
            .and_then(|file| file.strip_suffix(".rs"))
        {
            crate_root.push_str(&format!(" mod {name};"));
        }
    }
    let mut sources = vec![
        ("sample/src/lib.rs".to_owned(), crate_root),
        (
            "sample/src/sql_alias.rs".to_owned(),
            "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;".to_owned(),
        ),
        (
            "sample/src/plain.rs".to_owned(),
            "pub const MERGE_SQL: &str = \"SELECT 1\";".to_owned(),
        ),
        ("sample/src/writer.rs".to_owned(), writer.to_owned()),
    ];
    sources.extend(
        extra
            .iter()
            .map(|(path, source)| ((*path).to_owned(), (*source).to_owned())),
    );
    sources
}

// A path that reaches the re-exported statement through a module, rather than
// by the statement's own name, must still reach the census.
#[test]
fn module_paths_to_reexported_note_sql_are_reported() {
    let facade_alias = [("sample/src/facade.rs", "pub use crate::sql_alias as db;")];
    let facade_glob = [("sample/src/facade.rs", "pub use crate::sql_alias::*;")];
    let other_crate = [(
        "dbx/src/lib.rs",
        "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;",
    )];
    // (description, writer source, extra sample files)
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
    let helper_fn = [("sample/src/helpers.rs", "pub fn dbx() {}")];
    let cases: [Case; 14] = [
        (
            "renamed module import",
            "use crate::sql_alias as db;
             fn write(conn: &Connection) { conn.prepare_cached(db::MERGE_SQL); }",
            &[],
        ),
        (
            "module import",
            "use crate::sql_alias;
             fn write(conn: &Connection) { conn.prepare_cached(sql_alias::MERGE_SQL); }",
            &[],
        ),
        (
            "self import",
            "use crate::sql_alias::{self as db};
             fn write(conn: &Connection) { conn.prepare_cached(db::MERGE_SQL); }",
            &[],
        ),
        (
            "import through a module alias",
            "use crate::sql_alias as db;
             use db::MERGE_SQL as SQL;
             fn write(conn: &Connection) { conn.prepare_cached(SQL); }",
            &[],
        ),
        (
            "block-level module alias",
            "fn write(conn: &Connection) {
                 use crate::sql_alias as db;
                 conn.prepare_cached(db::MERGE_SQL);
             }",
            &[],
        ),
        (
            "value binding of the alias name",
            "use crate::sql_alias as db;
             fn write(conn: &Connection, db: u8) { conn.prepare_cached(db::MERGE_SQL); }",
            &[],
        ),
        (
            "module re-exported part-way along the path",
            "fn write(conn: &Connection) { conn.prepare_cached(crate::facade::db::MERGE_SQL); }",
            &facade_alias,
        ),
        (
            "glob re-export",
            "fn write(conn: &Connection) { conn.prepare_cached(crate::facade::MERGE_SQL); }",
            &facade_glob,
        ),
        (
            "workspace crate",
            "fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "workspace crate imported by name",
            "use dbx;
             fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "workspace crate imported under another name",
            "use dbx as db;
             fn write(conn: &Connection) { conn.prepare_cached(db::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "unrenamed self import",
            "use crate::sql_alias::{self};
             fn write(conn: &Connection) { conn.prepare_cached(sql_alias::MERGE_SQL); }",
            &[],
        ),
        // A function lives in the value namespace, so it never hides a crate.
        (
            "workspace crate behind a scanned function of the same name",
            "use crate::helpers::dbx;
             fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &[helper_fn[0], other_crate[0]],
        ),
        (
            "import path through a scanned function of the same name",
            "use crate::helpers::dbx;
             use dbx::MERGE_SQL as merge;
             fn write(conn: &Connection) { conn.prepare_cached(merge); }",
            &[helper_fn[0], other_crate[0]],
        ),
    ];
    for (form, writer, extra) in cases {
        let sites = scan_sources(&module_path_sources(writer, extra)).unwrap();
        assert_eq!(sites.len(), 1, "{form}: {sites:?}");
        assert_eq!(sites[0].key, "sample/src/writer.rs::write", "{form}");
        assert!(
            sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"),
            "{form}: {sites:?}"
        );
        assert!(
            check_inventory(&sites, &[], 0)
                .unwrap_err()
                .contains("unmapped sample/src/writer.rs::write"),
            "{form}"
        );
    }

    let relative = scan_sources(&[
        (
            "sample/src/lib.rs".into(),
            "mod sql_alias;
             fn write(conn: &Connection) { conn.prepare_cached(sql_alias::MERGE_SQL); }"
                .into(),
        ),
        (
            "sample/src/sql_alias.rs".into(),
            "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;".into(),
        ),
    ])
    .unwrap();
    assert_eq!(relative.len(), 1, "child module path: {relative:?}");
    assert_eq!(relative[0].key, "sample/src/lib.rs::write");
    assert!(relative[0]
        .evidence
        .contains("SQL constant NOTE_UPSERT_SQL"));
}

/// A writer module that reserves the note properties, with the given items and
/// imports ahead of a function that prepares the given SQL expression.
fn reserved_note_writer(items: &str, sql: &str) -> String {
    format!(
        "{items}
         fn write(conn: &Connection) {{
             reject_reserved_secret_gate_property(merged_props);
             conn.prepare_cached({sql});
         }}"
    )
}

/// The workspace crate `Ty`, which re-exports the note upsert statement.
const TY_CRATE: (&str, &str) = (
    "Ty/src/lib.rs",
    "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;",
);

#[test]
fn canonical_note_sql_names_on_scanned_types_are_not_routes() {
    let route = RouteInventoryEntry {
        site: "sample/src/writer.rs::write",
        ..qualified_reexport_route()
    };
    for constant in NOTE_PROPERTY_SQL_CONSTANTS {
        let associated =
            format!("pub struct Ty; impl Ty {{ pub const {constant}: &str = \"SELECT 1\"; }}");
        let type_source = [("sample/src/types.rs", associated.as_str())];
        for (form, writer, extra) in [
            (
                "local type",
                reserved_note_writer(&associated, &format!("Ty::{constant}")),
                &[][..],
            ),
            (
                "imported type",
                reserved_note_writer("use crate::types::Ty;", &format!("Ty::{constant}")),
                &type_source[..],
            ),
            (
                "glob-imported type",
                reserved_note_writer("use crate::types::*;", &format!("Ty::{constant}")),
                &type_source[..],
            ),
            (
                "type alias",
                reserved_note_writer(
                    &format!("{} type Ty = Base;", associated.replace("Ty", "Base")),
                    &format!("Ty::{constant}"),
                ),
                &[][..],
            ),
        ] {
            let sites = scan_sources(&module_path_sources(&writer, extra)).unwrap();
            assert!(sites.is_empty(), "{constant}, {form}: {sites:?}");
            assert!(check_inventory(&sites, &[route], 0)
                .unwrap_err()
                .contains("orphan route"));
        }
        let writer = reserved_note_writer("", &format!("khive_db::stores::note::{constant}"));
        let sites = scan_sources(&module_path_sources(&writer, &[])).unwrap();
        assert_eq!(sites.len(), 1, "known origin {constant}: {sites:?}");
        assert!(check_inventory(&sites, &[route], 0).is_ok());

        let local_module = format!(
            "mod khive_db {{ pub mod stores {{ pub mod note {{ pub const {constant}: &str = \"SELECT 1\"; }} }} }}
             fn read(conn: &Connection) {{ conn.prepare_cached(khive_db::stores::note::{constant}); }}"
        );
        let sites = scan_sources(&[("sample/src/lib.rs".into(), local_module)]).unwrap();
        assert!(sites.is_empty(), "local module {constant}: {sites:?}");
    }
}

#[test]
fn each_note_sql_reference_requires_its_own_resolution() {
    let route = RouteInventoryEntry {
        site: "sample/src/writer.rs::write",
        ..qualified_reexport_route()
    };
    for constant in NOTE_PROPERTY_SQL_CONSTANTS {
        let extra = [(
            "dbx/src/lib.rs",
            format!("pub use khive_db::stores::note::{constant} as MERGE_SQL;"),
        )];
        let extra_refs = [(extra[0].0, extra[0].1.as_str())];
        let resolved = "conn.prepare_cached(crate::sql_alias::MERGE_SQL);";
        let unresolved = "{ use external::Ty as dbx; conn.prepare_cached(dbx::MERGE_SQL); }";
        for (first, second) in [(resolved, unresolved), (unresolved, resolved)] {
            let writer = format!(
                "fn write(conn: &Connection) {{ reject_reserved_secret_gate_property(merged_props); {first} {second} }}"
            );
            let mut sources = module_path_sources(&writer, &extra_refs);
            for (_, source) in &mut sources {
                *source = source.replace("NOTE_UPSERT_SQL", constant);
            }
            let sites = scan_sources(&sources).unwrap();
            assert_eq!(sites.len(), 1, "{constant}: {sites:?}");
            assert!(sites[0]
                .evidence
                .contains(&format!("SQL constant {constant}")));
            assert!(sites[0]
                .evidence
                .contains(&format!("{UNRESOLVED_CONSTANT} {constant}")));
            assert!(check_inventory(&sites, &[route], 0)
                .unwrap_err()
                .contains("cannot tell whether this site reaches a note SQL constant"));
        }
        // Two independently resolved occurrences satisfy the same row.
        let writer = format!(
            "fn write(conn: &Connection) {{ reject_reserved_secret_gate_property(merged_props); {resolved} {resolved} }}"
        );
        let sites = scan_sources(&module_path_sources(&writer, &extra_refs)).unwrap();
        let both = RouteInventoryEntry {
            expected_writes: 2,
            ..route
        };
        assert!(check_inventory(&sites, &[both], 0).is_ok());
    }
}

// A type lives in the module namespace, so an imported type hides a same-named
// child module or workspace crate at the head of a path: `Ty::NAME` is the
// type's associated item, never an item of a crate `Ty`. The census resolves
// that path through a type declared in a scanned module, so it names no route.
// It cannot see what an import from outside the scanned sources is, so it
// refuses a path that would reach a note SQL constant through a same-named
// module or crate instead of routing or dropping it.
#[test]
fn an_import_that_is_not_a_module_never_routes_to_a_same_named_crate() {
    let type_crate = [TY_CRATE, ("sample/src/types.rs", "pub struct Ty;")];
    let route = RouteInventoryEntry {
        site: "sample/src/writer.rs::write",
        ..qualified_reexport_route()
    };
    let writer = reserved_note_writer;

    // The imported type `Ty` is not a module. Rust resolves the path through
    // the type, so the crate `Ty` is not reached and nothing is routed.
    for (form, imports) in [
        ("type import", "use crate::types::Ty;"),
        ("glob import", "use crate::types::*;"),
    ] {
        let through_type = scan_sources(&module_path_sources(
            &writer(imports, "Ty::MERGE_SQL"),
            &type_crate,
        ))
        .unwrap();
        assert!(
            through_type.is_empty(),
            "{form}: routed past the type: {through_type:?}"
        );
    }

    // Positive controls: the crate itself, by import and by bare path, is
    // still the crate, and the inventoried route passes.
    for (form, imports) in [("crate import", "use Ty;"), ("bare crate path", "")] {
        let through_crate = scan_sources(&module_path_sources(
            &writer(imports, "Ty::MERGE_SQL"),
            &type_crate,
        ))
        .unwrap();
        assert_eq!(through_crate.len(), 1, "{form}: {through_crate:?}");
        assert!(
            through_crate[0]
                .evidence
                .contains("SQL constant NOTE_UPSERT_SQL"),
            "{form}: {through_crate:?}"
        );
        assert!(
            check_inventory(&through_crate, &[route], 0).is_ok(),
            "{form}"
        );
    }

    let other_crate = [(
        "dbx/src/lib.rs",
        "pub mod sub;
         pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;",
    )];
    let other_crate_with_module = [
        other_crate[0],
        (
            "dbx/src/sub.rs",
            "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;",
        ),
    ];
    let facade = [
        other_crate[0],
        other_crate_with_module[1],
        (
            "sample/src/facade.rs",
            "use std::collections::HashMap as dbx;
             pub use dbx::sub as db;",
        ),
    ];
    // (description, writer source, extra files)
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
    let cases: [Case; 8] = [
        (
            "type import under the crate's name",
            "use std::collections::HashMap as dbx;
             fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "function import under the crate's name",
            "use std::cmp::max as dbx;
             fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "import path through a type import",
            "use std::collections::HashMap as dbx;
             use dbx::MERGE_SQL as merge;
             fn write(conn: &Connection) { conn.prepare_cached(merge); }",
            &other_crate,
        ),
        (
            "import path through a function import",
            "use std::cmp::max as dbx;
             use dbx::MERGE_SQL as merge;
             fn write(conn: &Connection) { conn.prepare_cached(merge); }",
            &other_crate,
        ),
        (
            "module of the crate behind a type import",
            "use std::collections::HashMap as dbx;
             use dbx::sub as db;
             fn write(conn: &Connection) { conn.prepare_cached(db::MERGE_SQL); }",
            &other_crate_with_module,
        ),
        (
            "module re-exported from a type import",
            "fn write(conn: &Connection) { conn.prepare_cached(crate::facade::db::MERGE_SQL); }",
            &facade,
        ),
        (
            "glob import the census cannot open",
            "use external::prelude::*;
             fn write(conn: &Connection) { conn.prepare_cached(dbx::MERGE_SQL); }",
            &other_crate,
        ),
        (
            "block-level type import",
            "fn write(conn: &Connection) {
                 use std::collections::HashMap as dbx;
                 conn.prepare_cached(dbx::MERGE_SQL);
             }",
            &other_crate,
        ),
    ];
    for (form, writer, extra) in cases {
        let sites = scan_sources(&module_path_sources(writer, extra)).unwrap();
        assert_eq!(sites.len(), 1, "{form}: {sites:?}");
        assert_eq!(sites[0].key, "sample/src/writer.rs::write", "{form}");
        assert!(
            !sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"),
            "{form}: routed to the crate: {sites:?}"
        );
        assert!(
            sites[0]
                .evidence
                .contains("UNRESOLVED note SQL constant NOTE_UPSERT_SQL"),
            "{form}: {sites:?}"
        );
        // An inventoried row does not excuse it.
        assert!(
            check_inventory(&sites, &[route], 0)
                .unwrap_err()
                .contains("cannot tell whether this site reaches a note SQL constant"),
            "{form}"
        );
    }

    // A non-module import or an opaque glob refuses only a path that would
    // reach a note SQL constant through a same-named module or crate.
    for (form, writer) in [
        (
            "type import with no crate of that name",
            "use std::collections::HashMap as other;
             fn write(conn: &Connection) { conn.prepare_cached(other::MERGE_SQL); }",
        ),
        (
            "opaque glob with no crate of that name",
            "use external::prelude::*;
             fn write(conn: &Connection) { conn.prepare_cached(other::MERGE_SQL); }",
        ),
    ] {
        let sites = scan_sources(&module_path_sources(writer, &other_crate)).unwrap();
        assert!(sites.is_empty(), "{form}: {sites:?}");
    }
}

// A type with an associated constant named like a workspace crate's note SQL
// constant: `Ty::MERGE_SQL` is the associated constant, whatever crate `Ty`
// exists. The path must not become a route to the crate's constant, so the
// inventory row that names the crate's route finds no site and stays an orphan.
#[test]
fn an_associated_constant_never_satisfies_an_inventory_row_for_a_crate_constant() {
    let route = RouteInventoryEntry {
        site: "sample/src/writer.rs::write",
        ..qualified_reexport_route()
    };
    let associated = "pub struct Ty;
         impl Ty { pub const MERGE_SQL: &str = \"SELECT 1\"; }";
    let types_module = [TY_CRATE, ("sample/src/types.rs", associated)];
    // (label, writer source, extra crate files)
    type Case<'a> = (&'a str, String, &'a [(&'a str, &'a str)]);
    let cases: [Case<'_>; 5] = [
        (
            "type declared in the writer module",
            reserved_note_writer(associated, "Ty::MERGE_SQL"),
            &[TY_CRATE],
        ),
        (
            "type declared in an inline module",
            reserved_note_writer(
                &format!("mod types {{ {associated} }} use types::Ty;"),
                "Ty::MERGE_SQL",
            ),
            &[TY_CRATE],
        ),
        (
            "type imported from another module",
            reserved_note_writer("use crate::types::Ty;", "Ty::MERGE_SQL"),
            &types_module,
        ),
        (
            "type imported by a glob",
            reserved_note_writer("use crate::types::*;", "Ty::MERGE_SQL"),
            &types_module,
        ),
        (
            "type declared in the function body",
            format!(
                "fn write(conn: &Connection) {{
                     reject_reserved_secret_gate_property(merged_props);
                     {associated}
                     conn.prepare_cached(Ty::MERGE_SQL);
                 }}"
            ),
            &[TY_CRATE],
        ),
    ];
    for (form, writer, extra) in cases {
        let sites = scan_sources(&module_path_sources(&writer, extra)).unwrap();
        assert!(sites.is_empty(), "{form}: {sites:?}");
        let failure = check_inventory(&sites, &[route], 0).unwrap_err();
        assert!(
            failure.contains("orphan route curation.merge.note at sample/src/writer.rs::write"),
            "{form}: {failure}"
        );
    }

    // Control: with no type of that name the path is the crate's constant, and
    // the same row is satisfied.
    let sites = scan_sources(&module_path_sources(
        &reserved_note_writer("", "Ty::MERGE_SQL"),
        &[TY_CRATE],
    ))
    .unwrap();
    assert_eq!(sites.len(), 1, "{sites:?}");
    assert!(sites[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"));
    assert!(check_inventory(&sites, &[route], 0).is_ok());
}

// A module's own child module shadows a module of the same name that a glob
// import brings in, so the path resolves through the child.
#[test]
fn a_child_module_shadows_a_glob_imported_module_of_the_same_name() {
    let sources = |writer_children: &str| {
        let mut sources: Vec<(String, String)> = vec![
            ("sample/src/lib.rs".into(), "mod other; mod writer;".into()),
            ("sample/src/other.rs".into(), "pub mod db;".into()),
            (
                "sample/src/other/db.rs".into(),
                "pub use khive_db::stores::note::NOTE_INSERT_IF_ABSENT_SQL as MERGE_SQL;".into(),
            ),
            (
                "sample/src/writer.rs".into(),
                format!(
                    "use crate::other::*; {writer_children}
                     fn write(conn: &Connection) {{ conn.prepare_cached(db::MERGE_SQL); }}"
                ),
            ),
        ];
        if !writer_children.is_empty() {
            sources.push((
                "sample/src/writer/db.rs".into(),
                "pub use khive_db::stores::note::NOTE_UPSERT_SQL as MERGE_SQL;".into(),
            ));
        }
        sources
    };

    let shadowed = scan_sources(&sources("mod db;")).unwrap();
    assert_eq!(
        shadowed.len(),
        1,
        "child module shadows the glob: {shadowed:?}"
    );
    assert_eq!(shadowed[0].key, "sample/src/writer.rs::write");
    assert!(shadowed[0]
        .evidence
        .contains("SQL constant NOTE_UPSERT_SQL"));
    assert!(!shadowed[0].evidence.contains("NOTE_INSERT_IF_ABSENT_SQL"));

    // Control: with no child of that name the glob-imported module is the
    // one Rust resolves, so the route reaches its constant instead.
    let through_glob = scan_sources(&sources("")).unwrap();
    assert_eq!(
        through_glob.len(),
        1,
        "the glob is followed: {through_glob:?}"
    );
    assert!(through_glob[0]
        .evidence
        .contains("SQL constant NOTE_INSERT_IF_ABSENT_SQL"));
}

// Each fixture names `MERGE_SQL` through a module path that Rust resolves to
// the unrelated constant, so reporting it would be a false route.
#[test]
fn module_paths_to_unrelated_names_are_not_reported() {
    for (form, writer) in [
        (
            "module without the statement",
            "use crate::plain as db;
             fn write(conn: &Connection) { conn.prepare_cached(db::MERGE_SQL); }",
        ),
        (
            "block import shadows the module alias",
            "use crate::sql_alias as db;
             fn write(conn: &Connection) {
                 use crate::plain as db;
                 conn.prepare_cached(db::MERGE_SQL);
             }",
        ),
        (
            "explicit import shadows a glob import",
            "use crate::sql_alias::*;
             use crate::plain::MERGE_SQL;
             fn write(conn: &Connection) { conn.prepare_cached(MERGE_SQL); }",
        ),
    ] {
        let sites = scan_sources(&module_path_sources(writer, &[])).unwrap();
        assert!(sites.is_empty(), "{form}: {sites:?}");
    }
}

fn synthetic_fixed_key_route(key_paths: &'static [&'static str]) -> RouteInventoryEntry {
    RouteInventoryEntry {
        id: "synthetic.fixed-keys",
        site: "sample/src/lib.rs::write",
        write_class: WriteClass::FixedKeySet { key_paths },
        reservation: Reservation::ByConstruction,
        ..qualified_reexport_route()
    }
}

fn synthetic_sql_sites(sql: &str) -> Vec<Site> {
    scan_sources(&[(
        "sample/src/lib.rs".into(),
        format!("fn write() {{ let statement = {sql:?}; }}"),
    )])
    .unwrap()
}

#[test]
fn fixed_key_sql_routes_require_the_complete_literal_key_set() {
    let expected = BTreeSet::from([
        "$.channel_slug".to_owned(),
        "$.quarantine_content_ref".to_owned(),
    ]);
    let route = synthetic_fixed_key_route(&["$.quarantine_content_ref", "$.channel_slug"]);
    for sql in [
        "UPDATE notes SET properties = json_set(properties, '$.channel_slug', ?1, '$.quarantine_content_ref', ?2) WHERE id = ?3",
        "UPDATE notes SET properties = json_set(properties, '$.quarantine_content_ref', '$.value_is_not_a_path', '$.channel_slug', ?2, '$.channel_slug', ?4), updated_at = MAX(updated_at, ?5) WHERE json_extract(properties, '$.predicate_is_not_a_write') = ?6",
        "UPDATE notes SET properties = json_remove(properties, '$.channel_slug', '$.quarantine_content_ref') WHERE id = ?1",
        "UPDATE notes SET properties = json_remove(json_set(properties, '$.channel_slug', ?1), '$.quarantine_content_ref') WHERE id = ?2",
        "UPDATE notes SET properties = json_set(json_remove(properties, '$.quarantine_content_ref'), '$.channel_slug', ?1) WHERE id = ?2",
        "UpDaTe notes SeT properties = JSON_SET ( properties , '$.channel_slug', CASE WHEN ?1 THEN json_extract(properties, '$.read_only') ELSE ?2 END, '$.quarantine_content_ref', ?3 ) WHERE id = ?4",
    ] {
        assert_eq!(sql_fixed_key_paths(sql), Some(expected.clone()), "{sql}");
        let sites = synthetic_sql_sites(sql);
        assert_eq!(sites.len(), 1, "{sql}: {sites:?}");
        assert_eq!(sites[0].class, DetectedClass::FixedKeySet(expected.clone()), "{sql}");
        assert!(check_inventory(&sites, &[route], 0).is_ok(), "{sql}");
        for row in [
            synthetic_fixed_key_route(&["$.channel_slug"]),
            synthetic_fixed_key_route(&["$.channel_slug", "$.different"]),
            synthetic_fixed_key_route(&["$.channel_slug", "$.quarantine_content_ref", "$.extra"]),
            synthetic_fixed_key_route(&[]),
        ] {
            assert!(check_inventory(&sites, &[row], 0).unwrap_err().contains("write class"), "{sql}: {row:?}");
        }
        let unchecked = RouteInventoryEntry {
            reservation: qualified_reexport_route().reservation,
            ..route
        };
        assert!(check_inventory(&sites, &[unchecked], 0).unwrap_err()
            .contains("fixed-key-set route must be reserved by construction"));
    }

    // A newly introduced third key is not covered by the two-key declaration.
    let extra = synthetic_sql_sites("UPDATE notes SET properties = json_set(properties, '$.channel_slug', ?1, '$.quarantine_content_ref', ?2, '$.extra', ?3) WHERE id = ?4");
    assert!(check_inventory(&extra, &[route], 0)
        .unwrap_err()
        .contains("write class"));

    // A second statement in the same function also contributes its write key.
    let source = "fn write() { let first = \"UPDATE notes SET properties = json_set(properties, '$.channel_slug', ?1) WHERE id = ?2\"; let second = \"UPDATE notes SET properties = json_remove(properties, '$.quarantine_content_ref') WHERE id = ?2\"; }";
    let combined = scan_sources(&[("sample/src/lib.rs".into(), source.into())]).unwrap();
    assert_eq!(combined[0].class, DetectedClass::FixedKeySet(expected));
    let both = RouteInventoryEntry {
        expected_writes: 2,
        ..route
    };
    assert!(check_inventory(&combined, &[both], 0).is_ok());
}

#[test]
fn one_unique_literal_key_remains_a_single_key_and_preserves_case() {
    for sql in [
        "UPDATE notes SET properties = json_set(properties, '$.Foo', '$.value', '$.Foo', ?2) WHERE json_extract(properties, '$.where_only') = ?3",
        "UPDATE notes SET properties = json_remove(properties, '$.Foo', '$.Foo') WHERE id = ?1",
        "UPDATE notes SET properties = json_set(json_remove(properties, '$.Foo'), '$.Foo', ?1) WHERE id = ?2",
    ] {
        let sites = synthetic_sql_sites(sql);
        assert_eq!(sites[0].class, DetectedClass::SingleKey("$.Foo".into()), "{sql}");
        let single = RouteInventoryEntry {
            write_class: WriteClass::SingleKey { key_path: "$.Foo" },
            ..synthetic_fixed_key_route(&["$.Foo"])
        };
        assert!(check_inventory(&sites, &[single], 0).is_ok());
        assert!(check_inventory(&sites, &[synthetic_fixed_key_route(&["$.Foo"])], 0).is_ok());
        assert!(check_inventory(&sites, &[synthetic_fixed_key_route(&["$.foo"])], 0)
            .unwrap_err().contains("write class"));
        assert!(check_inventory(&sites, &[synthetic_fixed_key_route(&["Foo"])], 0)
            .unwrap_err().contains("write class"));
    }
}

#[test]
fn uncertain_sql_key_sets_require_whole_object_reservation() {
    let route = synthetic_fixed_key_route(&["$.safe", "$.other"]);
    for expression in [
        "json_set(properties, ?1, ?2)",
        "json_set(properties, '$.' || ?1, ?2)",
        "json_remove(properties, json_extract(?1, '$.path'))",
        "json_set(properties, '$', ?1)",
        "json_set(properties, '$.nested.key', ?1)",
        "json_set(properties, '$.\"quoted\"', ?1)",
        "json_remove(properties, '$[\"bracket\"]')",
        "json_set(properties, '$.safe[0]', ?1)",
        "json_set(properties, '$.safe-key', ?1)",
        "json_set(properties, '', ?1)",
        "json_set(properties, '$.safe', ?1, ?2, ?3)",
        "json_remove(properties, '$.safe', ?1)",
        "json_set(properties)",
        "json_remove(properties)",
        "properties",
        "json_set(other_document, '$.safe', ?1)",
        "json_set(coalesce(properties, '{}'), '$.safe', ?1)",
        "json_set(json_patch(properties, ?1), '$.safe', ?2)",
        "json_patch(properties, ?1)",
        "json_insert(properties, '$.safe', ?1)",
        "json_replace(properties, '$.safe', ?1)",
        "json_set(properties, '$.safe', ?1) || ?2",
    ] {
        let sql = format!("UPDATE notes SET properties = {expression} WHERE id = ?9");
        assert_eq!(sql_fixed_key_paths(&sql), None, "{sql}");
        let sites = synthetic_sql_sites(&sql);
        assert_eq!(sites[0].class, DetectedClass::WholeObject, "{sql}");
        assert!(
            check_inventory(&sites, &[route], 0)
                .unwrap_err()
                .contains("write class"),
            "{sql}"
        );
    }
    for sql in [
        "UPDATE notes SET properties = json_set(properties, '$.safe', ?1), properties = ?2 WHERE id = ?3",
        "UPDATE notes SET properties = json_set(properties, '$.safe', ?1); UPDATE notes SET properties = ?2",
        "UPDATE notes SET properties = json_set(properties, '$.safe', ?1) /* opaque */ WHERE id = ?2",
    ] {
        let sites = synthetic_sql_sites(sql);
        assert_eq!(sites[0].class, DetectedClass::WholeObject, "{sql}");
        assert!(check_inventory(&sites, &[route], 0).is_err(), "{sql}");
    }
    // An invalid declared path or empty set cannot match an opaque write.
    let opaque = synthetic_sql_sites("UPDATE notes SET properties = ?1 WHERE id = ?2");
    for row in [
        synthetic_fixed_key_route(&[]),
        synthetic_fixed_key_route(&["$.nested.key"]),
    ] {
        assert!(check_inventory(&opaque, &[row], 0)
            .unwrap_err()
            .contains("write class"));
    }
}

#[test]
fn conservative_whole_object_rows_still_require_their_named_check() {
    let row = RouteInventoryEntry {
        site: "sample/src/lib.rs::write",
        ..*ROUTE_INVENTORY
            .iter()
            .find(|row| row.id == "pending.outcome")
            .expect("pending outcome retains whole-object coverage")
    };
    assert_eq!(row.write_class, WriteClass::WholeObject);
    for expression in [
        "json_set(properties, '$.dispatch_receipt', json(?1), '$.lease_expires_at', ?2)",
        "json_remove(json_set(properties, '$.status', 'pending'), '$.firing_at', '$.lease_expires_at')",
    ] {
        let sql = format!("UPDATE notes SET properties = {expression}, updated_at = ?3 WHERE id = ?4");
        let checked = format!(
            "fn write() {{ check_fixed_path_whole_object_snapshot(snapshot); let statement = {sql:?}; }}"
        );
        let sites = scan_sources(&[("sample/src/lib.rs".into(), checked.clone())]).unwrap();
        assert!(matches!(sites[0].class, DetectedClass::FixedKeySet(_)));
        assert!(check_inventory(&sites, &[row], 0).is_ok());
        let unchecked = checked.replace("check_fixed_path_whole_object_snapshot(snapshot);", "");
        let sites = scan_sources(&[("sample/src/lib.rs".into(), unchecked)]).unwrap();
        assert!(check_inventory(&sites, &[row], 0).unwrap_err()
            .contains("whole-object write lacks its named check/callee"));
        let by_construction = RouteInventoryEntry {
            reservation: Reservation::ByConstruction,
            ..row
        };
        assert!(check_inventory(&sites, &[by_construction], 0).unwrap_err()
            .contains("whole-object write lacks its named check/callee"));
    }
}

#[test]
fn every_spelling_of_a_guarded_table_write_is_reported() {
    for (sql, target) in [
        (
            "UPDATE main.notes SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE temp.entities SET properties = ?1 WHERE id = ?2",
            Substrate::Entity,
        ),
        (
            "UPDATE OR IGNORE notes SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE OR ABORT entities SET properties = ?1 WHERE id = ?2",
            Substrate::Entity,
        ),
        (
            "INSERT OR ABORT INTO notes (id, properties) VALUES (?1, ?2)",
            Substrate::Note,
        ),
        (
            "INSERT OR FAIL INTO entities (id, properties) VALUES (?1, ?2)",
            Substrate::Entity,
        ),
        (
            "INSERT OR ROLLBACK INTO notes (id, properties) VALUES (?1, ?2)",
            Substrate::Note,
        ),
        (
            "UPDATE notes AS n SET properties = ?1 WHERE n.id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE notes INDEXED BY idx SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE /* c */ notes SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "INSERT INTO main.entities (id, properties) VALUES (?1, ?2)",
            Substrate::Entity,
        ),
        (
            "UPDATE notes SET other = (SELECT 1 WHERE 0), properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE entities SET kind = 'where', properties = ?1 WHERE id = ?2",
            Substrate::Entity,
        ),
    ] {
        assert_eq!(sql_target(sql), Some(target), "{sql}");
        assert_eq!(sql_unclassified_write(sql), None, "{sql}");
        let sites = synthetic_sql_sites(sql);
        assert_eq!(sites.len(), 1, "{sql}: {sites:?}");
        assert_eq!(sites[0].target, target, "{sql}");
        assert!(
            check_inventory(&sites, &[], 0)
                .unwrap_err()
                .contains("unmapped sample/src/lib.rs::write"),
            "{sql}"
        );
    }
}

#[test]
fn a_guarded_table_write_the_census_cannot_classify_is_refused() {
    for (sql, target) in [
        (
            "UPDATE other.notes SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "UPDATE notes mystery tail SET properties = ?1 WHERE id = ?2",
            Substrate::Note,
        ),
        (
            "INSERT INTO other.entities (id, properties) VALUES (?1, ?2)",
            Substrate::Entity,
        ),
        (
            "INSERT OR REPLACE INTO other.notes (id, properties) VALUES (?1, ?2)",
            Substrate::Note,
        ),
    ] {
        assert_eq!(sql_target(sql), None, "{sql}");
        assert_eq!(sql_unclassified_write(sql), Some(target), "{sql}");
        let sites = synthetic_sql_sites(sql);
        assert_eq!(sites.len(), 1, "{sql}: {sites:?}");
        assert_eq!(sites[0].target, target, "{sql}");
        let failure = check_inventory(&sites, &[], 0).unwrap_err();
        assert!(
            failure.contains(
                "sample/src/lib.rs::write: cannot classify this write to a guarded table"
            ),
            "{sql}: {failure}"
        );
        assert!(failure.contains(&format!("{sql:?}")), "{sql}: {failure}");
    }

    // An inventory row that would otherwise accept the site cannot excuse a
    // statement the census could not read, while the same write through a
    // schema the census understands is accepted by that row.
    let row = RouteInventoryEntry {
        site: "sample/src/lib.rs::write",
        ..*ROUTE_INVENTORY
            .iter()
            .find(|row| row.id == "pending.outcome")
            .expect("pending outcome retains whole-object coverage")
    };
    let checked = |sql: &str| {
        let source = format!(
            "fn write() {{ check_fixed_path_whole_object_snapshot(snapshot); let statement = {sql:?}; }}"
        );
        scan_sources(&[("sample/src/lib.rs".into(), source)]).unwrap()
    };
    let understood = checked("UPDATE main.notes SET properties = ?1 WHERE id = ?2");
    assert!(check_inventory(&understood, &[row], 0).is_ok());
    let unreadable = checked("UPDATE other.notes SET properties = ?1 WHERE id = ?2");
    let failure = check_inventory(&unreadable, &[row], 0).unwrap_err();
    assert!(
        failure.contains("cannot classify this write to a guarded table"),
        "{failure}"
    );
    assert!(!failure.contains("unmapped"), "{failure}");
}

#[test]
fn migration_sql_reports_every_guarded_write_spelling() {
    for (statement, unclassified) in [
        (
            "UPDATE main.notes SET properties = '{}' WHERE id = 'fixture'",
            false,
        ),
        (
            "UPDATE /* c */ notes SET properties = '{}' WHERE id = 'fixture'",
            false,
        ),
        (
            "UPDATE notes AS n SET properties = '{}' WHERE n.id = 'fixture'",
            false,
        ),
        (
            "INSERT OR ABORT INTO temp.entities (id, properties) VALUES ('a', '{}')",
            false,
        ),
        (
            "UPDATE other.notes SET properties = '{}' WHERE id = 'fixture'",
            true,
        ),
    ] {
        let sites = scan_migration_sources(&[(
            "khive-db/sql/999-census-fixture.sql".into(),
            statement.into(),
        )]);
        assert_eq!(sites.len(), 1, "{statement}: {sites:?}");
        assert_eq!(sites[0].route_class, RouteClass::Migration, "{statement}");
        let failure = check_inventory(&sites, &[], 0).unwrap_err();
        assert!(
            failure.contains("unmapped khive-db/sql/999-census-fixture.sql::statement_1"),
            "{statement}: {failure}"
        );
        assert_eq!(
            failure.contains("cannot classify this write to a guarded table"),
            unclassified,
            "{statement}: {failure}"
        );
    }
}

#[test]
fn guarded_table_statements_that_write_no_properties_are_not_routes() {
    for sql in [
        "UPDATE OR IGNORE notes SET key = ?1 WHERE id = ?2 AND namespace = ?3 \
         AND kind = 'message' AND key IS NULL AND deleted_at IS NULL",
        "UPDATE OR IGNORE notes SET key = ?1 WHERE id = ?2 AND namespace = ?3 \
         AND kind = ?4 AND key IS NULL AND deleted_at IS NULL",
        "UPDATE other.notes SET updated_at = ?1 WHERE id = ?2",
        "INSERT INTO notes_seq (note_id) VALUES (?1)",
        "INSERT INTO audit SELECT id FROM notes",
        "CREATE TRIGGER guard BEFORE UPDATE OF content, properties ON notes \
         FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'refused'); END",
    ] {
        assert_eq!(sql_target(sql), None, "{sql}");
        assert_eq!(sql_unclassified_write(sql), None, "{sql}");
        assert!(synthetic_sql_sites(sql).is_empty(), "{sql}");
    }
}

#[test]
fn an_update_that_names_properties_after_set_is_reported_even_when_it_only_reads_them() {
    let sql = "UPDATE notes SET key = ?1 WHERE id = ?2 AND properties IS NULL";
    assert_eq!(sql_target(sql), Some(Substrate::Note), "{sql}");
    assert_eq!(sql_unclassified_write(sql), None, "{sql}");
    let sql = "UPDATE other.notes SET updated_at = ?1 WHERE properties IS NULL";
    assert_eq!(sql_target(sql), None, "{sql}");
    assert_eq!(sql_unclassified_write(sql), Some(Substrate::Note), "{sql}");
}
