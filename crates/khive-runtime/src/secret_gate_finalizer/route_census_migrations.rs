//! Migration SQL population for the route census: the files registered in
//! `khive-db/src/migrations.rs`, not every file under `khive-db/sql`.

use super::*;

fn registered_migration_paths(source: &str) -> Result<BTreeSet<String>, String> {
    let file = syn::parse_file(source).map_err(|error| format!("migrations.rs: {error}"))?;
    let mut constants = BTreeMap::new();
    for item in &file.items {
        if let syn::Item::Const(item) = item {
            if constants.insert(item.ident.to_string(), item).is_some() {
                return Err(format!("duplicate migration constant {}", item.ident));
            }
        }
    }
    let registry = constants.get("MIGRATIONS").ok_or("missing MIGRATIONS")?;
    let Expr::Reference(reference) = registry.expr.as_ref() else {
        return Err("MIGRATIONS must reference a literal array".into());
    };
    let Expr::Array(array) = reference.expr.as_ref() else {
        return Err("MIGRATIONS must reference a literal array".into());
    };
    if array.elems.is_empty() {
        return Err("MIGRATIONS must not be empty".into());
    }
    let mut paths = BTreeSet::new();
    for entry in &array.elems {
        let Expr::Struct(entry) = entry else {
            return Err("unrecognized VersionedMigration registration".into());
        };
        if !entry.path.is_ident("VersionedMigration")
            || entry.qself.is_some()
            || entry.rest.is_some()
        {
            return Err("unrecognized VersionedMigration registration".into());
        }
        let fields = entry
            .fields
            .iter()
            .filter(|field| matches!(&field.member, syn::Member::Named(name) if name == "up"))
            .collect::<Vec<_>>();
        let [field] = fields.as_slice() else {
            return Err("migration must have one up field".into());
        };
        let Expr::Path(up) = &field.expr else {
            return Err("migration up must name one include constant".into());
        };
        if up.qself.is_some() {
            return Err("unresolved migration up path".into());
        }
        let name = up.path.get_ident().ok_or("unresolved migration up path")?;
        let constant = constants
            .get(&name.to_string())
            .ok_or_else(|| format!("missing migration constant {name}"))?;
        let Expr::Macro(include) = constant.expr.as_ref() else {
            return Err(format!("unresolved migration include {name}"));
        };
        if !include.mac.path.is_ident("include_str") {
            return Err(format!("unresolved migration include {name}"));
        }
        let literal = syn::parse2::<syn::LitStr>(include.mac.tokens.clone())
            .map_err(|error| format!("migration include {name}: {error}"))?;
        let value = literal.value();
        let filename = value
            .strip_prefix("../sql/")
            .filter(|filename| {
                filename
                    .strip_suffix(".sql")
                    .is_some_and(|stem| !stem.is_empty())
                    && filename
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            })
            .ok_or_else(|| format!("migration include {name} must name a file in ../sql"))?;
        paths.insert(format!("khive-db/sql/{filename}"));
    }
    Ok(paths)
}

fn registered_migration_sources(
    source: &str,
    mut read: impl FnMut(&str) -> Result<String, String>,
) -> Result<Vec<(String, String)>, String> {
    registered_migration_paths(source)?
        .into_iter()
        .map(|path| read(&path).map(|source| (path, source)))
        .collect()
}

/// Files that khive-db code applies as migrations outside a registration's `up`
/// field: helper statements and staged steps are included from these sources.
const MIGRATION_RUNNERS: [&str; 2] = [
    "khive-db/src/migrations.rs",
    "khive-db/src/session_identity_migration.rs",
];

/// Every `include_str!("../sql/...")` in a runner source, wherever the macro
/// sits: a constant, a registration or a function body.
fn runner_include_paths(source: &str) -> Result<BTreeSet<String>, String> {
    struct Includes(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Includes {
        fn visit_macro(&mut self, mac: &'ast Macro) {
            if mac.path.is_ident("include_str") {
                if let Ok(literal) = syn::parse2::<syn::LitStr>(mac.tokens.clone()) {
                    if let Some(name) = literal.value().strip_prefix("../sql/") {
                        self.0.insert(format!("khive-db/sql/{name}"));
                    }
                }
            }
            syn::visit::visit_macro(self, mac);
        }
    }
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let mut includes = Includes(BTreeSet::new());
    includes.visit_file(&file);
    Ok(includes.0)
}

/// The SQL lint's content rule (`is_ddl` in scripts/lint-sql.sh): a file whose
/// first non-comment line opens with a schema keyword declares schema.
fn opens_with_schema_statement(sql: &str) -> bool {
    sql.lines()
        .find(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with("--")
        })
        .is_none_or(|line| {
            let word = line
                .trim_start()
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            ["CREATE", "ALTER", "DROP", "PRAGMA", "BEGIN", "COMMIT"]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
}

/// The SQL files that apply as migrations: every registered `up` file, every
/// file a migration runner includes, and every file that opens with a schema
/// statement. A query file the stores bind and run is none of these, like the
/// store literals it replaced.
fn migration_population(
    registrations: &str,
    runners: &[(&str, String)],
    sql_files: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut paths = registered_migration_paths(registrations)?;
    for (name, source) in runners {
        paths.extend(runner_include_paths(source).map_err(|error| format!("{name}: {error}"))?);
    }
    paths.extend(
        sql_files
            .iter()
            .filter(|(_, sql)| opens_with_schema_statement(sql))
            .map(|(path, _)| path.clone()),
    );
    paths
        .into_iter()
        .map(|path| {
            let sql = sql_files
                .get(&path)
                .ok_or_else(|| format!("missing {path}"))?;
            Ok((path, sql.clone()))
        })
        .collect()
}

pub(super) fn live_migration_sources() -> Vec<(String, String)> {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf();
    let read = |path: &str| {
        std::fs::read_to_string(crates.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
    };
    let runners = MIGRATION_RUNNERS
        .iter()
        .map(|path| (*path, read(path)))
        .collect::<Vec<_>>();
    let mut sql_files = BTreeMap::new();
    let sql_dir = crates.join("khive-db/sql");
    for entry in
        std::fs::read_dir(&sql_dir).unwrap_or_else(|error| panic!("{}: {error}", sql_dir.display()))
    {
        let path = entry.expect("SQL source entry").path();
        if path.extension().is_some_and(|extension| extension == "sql") {
            let name = path.file_name().expect("SQL source name").to_string_lossy();
            let key = format!("khive-db/sql/{name}");
            let sql = read(&key);
            sql_files.insert(key, sql);
        }
    }
    migration_population(&runners[0].1, &runners, &sql_files)
        .expect("resolve migration SQL population")
}

#[test]
fn migration_population_follows_registrations_instead_of_neighboring_queries() {
    let registry = r#"
        const RECONCILE_VERSION: u32 = 5;
        const RECONCILE: &str = include_str!("../sql/reconcile.sql");
        const APPLICATION: &str = include_str!("../sql/application.sql");
        const MIGRATIONS: &[VersionedMigration] = &[
            VersionedMigration { version: RECONCILE_VERSION, name: "reconcile", up: RECONCILE },
        ];
    "#;
    let sql = BTreeMap::from([
        (
            "khive-db/sql/reconcile.sql",
            "UPDATE notes SET properties = '{}' WHERE id = 'old'; CREATE INDEX after_reconciliation ON notes(id);",
        ),
        (
            "khive-db/sql/application.sql",
            "INSERT INTO notes (id, properties) VALUES (?1, ?2);",
        ),
    ]);
    let read = |path: &str| {
        sql.get(path)
            .map(|source| (*source).to_owned())
            .ok_or_else(|| format!("missing {path}"))
    };
    let sources = registered_migration_sources(registry, read).unwrap();
    assert_eq!(sources.len(), 1);
    let sites = scan_migration_sources(&sources);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].key, "khive-db/sql/reconcile.sql::statement_1");
    assert_eq!(sites[0].route_class, RouteClass::Migration);
    let registered_application = registry.replace("up: RECONCILE", "up: APPLICATION");
    let sources = registered_migration_sources(&registered_application, read).unwrap();
    let sites = scan_migration_sources(&sources);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].key, "khive-db/sql/application.sql::statement_1");
    assert_eq!(sites[0].route_class, RouteClass::Migration);
}

#[test]
fn migration_population_refuses_unresolved_or_missing_registrations() {
    let registry = r#"
        const UP: &str = include_str!("../sql/migration.sql");
        const MIGRATIONS: &[VersionedMigration] = &[
            VersionedMigration { version: 1, name: "fixture", up: UP },
        ];
    "#;
    assert!(registered_migration_paths(&registry.replace("up: UP", "up: UNKNOWN")).is_err());
    assert!(registered_migration_paths(&registry.replace("../sql/", "../../")).is_err());
    assert!(registered_migration_sources(registry, |_| Err("missing SQL".into())).is_err());
}

#[test]
fn migration_population_keeps_runner_helpers_and_schema_files_but_not_queries() {
    let registry = r#"
        const RECONCILE: &str = include_str!("../sql/reconcile.sql");
        const MIGRATIONS: &[VersionedMigration] = &[
            VersionedMigration { version: 5, name: "reconcile", up: RECONCILE },
        ];
    "#;
    let helper = r#"
        fn stage(conn: &Connection) {
            conn.execute_batch(include_str!("../sql/stage.sql")).unwrap();
        }
    "#;
    let sql = BTreeMap::from([
        (
            "khive-db/sql/reconcile.sql".to_string(),
            "CREATE INDEX reconcile ON notes(id);".to_string(),
        ),
        (
            "khive-db/sql/stage.sql".to_string(),
            "UPDATE notes SET properties = '{}' WHERE id = 'staged';".to_string(),
        ),
        (
            "khive-db/sql/notes-ddl.sql".to_string(),
            "-- schema\nCREATE TABLE notes (id TEXT);\nUPDATE notes SET properties = '{}';"
                .to_string(),
        ),
        (
            "khive-db/sql/message-insert.sql".to_string(),
            "INSERT INTO notes (id, properties) VALUES (?1, ?2)".to_string(),
        ),
    ]);
    let runners = [
        ("migrations.rs", registry.to_string()),
        ("helper.rs", helper.to_string()),
    ];
    let sources = migration_population(registry, &runners, &sql).unwrap();
    let keys = scan_migration_sources(&sources)
        .into_iter()
        .map(|site| site.key)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "khive-db/sql/notes-ddl.sql::statement_2",
            "khive-db/sql/stage.sql::statement_1"
        ]
    );
    let missing = BTreeMap::from([(
        "khive-db/sql/reconcile.sql".to_string(),
        "CREATE INDEX reconcile ON notes(id);".to_string(),
    )]);
    assert!(migration_population(registry, &runners, &missing).is_err());
}

#[test]
fn schema_statement_rule_matches_the_sql_lint() {
    assert!(opens_with_schema_statement(
        "-- header\n\n  ALTER TABLE notes ADD COLUMN x TEXT;"
    ));
    assert!(opens_with_schema_statement("pragma foreign_keys = off;"));
    assert!(opens_with_schema_statement(
        "BEGIN;\nUPDATE notes SET kind = 'x';"
    ));
    assert!(opens_with_schema_statement(""));
    assert!(!opens_with_schema_statement(
        "-- header\nUPDATE notes SET properties = '{}';"
    ));
    assert!(!opens_with_schema_statement("CREATE_LOG_ROW;"));
}

#[test]
fn live_migration_population_keeps_runner_helpers_and_excludes_store_queries() {
    let paths = live_migration_sources()
        .into_iter()
        .map(|(path, _)| path)
        .collect::<BTreeSet<_>>();
    for kept in [
        "khive-db/sql/021-attachments-b-claim-fences.sql",
        "khive-db/sql/040b-session-source-scope-swap.sql",
        "khive-db/sql/044-comm-outbound-due-a-columns.sql",
        "khive-db/sql/notes-ddl.sql",
    ] {
        assert!(paths.contains(kept), "{kept} left the migration population");
    }
    assert!(!paths.contains("khive-db/sql/comm-recipient-message-note-insert.sql"));
}

#[test]
fn registered_migration_property_writer_inventory_is_unchanged() {
    let sites = scan_migration_sources(&live_migration_sources());
    let inventory = sites
        .into_iter()
        .map(|site| (site.key, site.target, site.write_count, site.route_class))
        .collect::<Vec<_>>();
    assert_eq!(
        inventory,
        vec![(
            "khive-db/sql/005-unique-comm-external-id.sql::statement_1".into(),
            Substrate::Note,
            1,
            RouteClass::Migration
        )]
    );
}
