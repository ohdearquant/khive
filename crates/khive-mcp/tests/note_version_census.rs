use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{params, Connection};
use syn::parse::Parser;
use syn::visit::Visit;

const DB: &str = "khive-db/src/stores/note.rs";
const EVENTS: &str = "khive-mcp/src/pending_events.rs";
const GTD: &str = "khive-pack-gtd/src/handlers.rs";
const SCHEDULE: &str = "khive-pack-schedule/src/handlers.rs";
const CURATION: &str = "khive-runtime/src/curation.rs";
const CREATE: &str = "khive-runtime/src/note_create.rs";
const FAULT: &str = "khive-runtime/src/atomic_message.rs";
const ID: &str = "00000000-0000-4000-8000-000000000001";

fn words(sql: &str) -> Vec<String> {
    let mut input = sql.chars().peekable();
    let mut words = Vec::new();
    while let Some(c) = input.next() {
        if c == '-' && input.peek() == Some(&'-') {
            for c in input.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if c == '/' && input.peek() == Some(&'*') {
            input.next();
            while let Some(c) = input.next() {
                if c == '*' && input.peek() == Some(&'/') {
                    input.next();
                    break;
                }
            }
        } else if matches!(c, '\'' | '"' | '`' | '[') {
            let close = if c == '[' { ']' } else { c };
            let mut quoted = String::new();
            while let Some(next) = input.next() {
                if next == close {
                    if close != ']' && input.peek() == Some(&close) {
                        input.next();
                    } else {
                        break;
                    }
                }
                quoted.push(next);
            }
            if c != '\'' {
                words.push(quoted.to_ascii_uppercase());
            }
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let mut word = String::from(c);
            while input
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                word.push(input.next().unwrap());
            }
            words.push(word.to_ascii_uppercase());
        }
    }
    words
}

fn note_writer(sql: &str) -> bool {
    let words = words(sql);
    words.iter().enumerate().any(|(i, word)| {
        if word != "UPDATE" {
            return false;
        }
        let mut target = i + 1;
        if words.get(target).is_some_and(|w| w == "OR") {
            target += 2;
        }
        if words.get(target).is_some_and(|w| w == "MAIN") {
            target += 1;
        }
        words.get(target).is_some_and(|w| w == "NOTES")
    }) || (note_insert_columns(&words).is_some() && words.windows(2).any(|w| w == ["DO", "UPDATE"]))
}

fn note_insert_columns(words: &[String]) -> Option<usize> {
    let insert = words.iter().position(|w| w == "INSERT")?;
    let into = insert + words[insert..].iter().position(|w| w == "INTO")?;
    let mut table = into + 1;
    if words.get(table).is_some_and(|w| w == "MAIN") {
        table += 1;
    }
    words
        .get(table)
        .is_some_and(|w| w == "NOTES")
        .then_some(table + 1)
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

#[derive(Default)]
struct Scanner {
    owner: String,
    statements: Vec<(String, String)>,
}

impl<'ast> Visit<'ast> for Scanner {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
            syn::visit::visit_item_fn(self, item);
            self.owner = old;
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
            syn::visit::visit_impl_item_fn(self, item);
            self.owner = old;
        }
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if !test_only(&item.attrs) {
            let old = std::mem::replace(&mut self.owner, item.ident.to_string());
            syn::visit::visit_item_const(self, item);
            self.owner = old;
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_impl(self, item);
        }
    }

    fn visit_expr_block(&mut self, item: &'ast syn::ExprBlock) {
        if !test_only(&item.attrs) {
            syn::visit::visit_expr_block(self, item);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // SQL commonly lives inside format!/vec!, whose bodies Visit leaves opaque.
        let args = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(mac.tokens.clone());
        match args {
            Ok(args) => {
                if mac.path.is_ident("concat") {
                    let mut combined = String::new();
                    for arg in &args {
                        if let syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) = arg
                        {
                            combined.push_str(&s.value());
                        } else {
                            assert!(
                                !combined.to_ascii_uppercase().contains("UPDATE"),
                                "uninspectable SQL concat in {}",
                                self.owner
                            );
                        }
                    }
                    self.visit_lit_str(&syn::LitStr::new(&combined, mac.bang_token.span));
                    return;
                }
                for arg in &args {
                    self.visit_expr(arg);
                }
            }
            Err(_) => assert!(
                !(mac
                    .tokens
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("update")
                    && mac
                        .tokens
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("notes")),
                "uninspectable note SQL in macro owned by {}",
                self.owner
            ),
        }
    }

    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        let sql = literal.value();
        let tokens = words(&sql);
        if tokens.first().is_some_and(|w| w == "UPDATE") && tokens.iter().any(|w| w == "SET") {
            let target = sql
                .split_once(" SET ")
                .map_or(sql.as_str(), |(target, _)| target);
            assert!(
                !target.contains('{'),
                "dynamic UPDATE target in {}: {sql}",
                self.owner
            );
        }
        if let Some(start) = note_insert_columns(&tokens) {
            let columns = &tokens[start..];
            let end = columns
                .iter()
                .position(|w| w == "VALUES" || w == "SELECT")
                .unwrap_or(columns.len());
            assert!(
                !columns[..end].iter().any(|w| w == "VERSION"),
                "{} explicitly initializes note.version",
                self.owner
            );
        }
        if note_writer(&sql) {
            assert!(!self.owner.is_empty(), "note writer needs a named owner");
            self.statements.push((self.owner.clone(), sql));
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}

fn files(dir: &Path, extension: &str, output: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if !matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("tests" | "target" | ".git")
            ) {
                files(&path, extension, output);
            }
        } else if path.extension().is_some_and(|e| e == extension)
            && !path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .ends_with("_tests")
        {
            output.push(path);
        }
    }
}

fn census() -> BTreeMap<(String, String), String> {
    let root = root();
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            files(&src, "rs", &mut sources);
        }
    }
    let mut found = BTreeMap::new();
    for source in sources {
        let mut scanner = Scanner::default();
        scanner.visit_file(&syn::parse_file(&std::fs::read_to_string(&source).unwrap()).unwrap());
        for (owner, sql) in scanner.statements {
            let key = (
                source
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                owner,
            );
            assert!(
                found.insert(key.clone(), sql).is_none(),
                "multiple writer templates: {key:?}"
            );
        }
    }
    let expected = [
        (DB, "NOTE_UPSERT_SQL"),
        (DB, "note_replace_if_unchanged_statement"),
        (DB, "note_metadata_replace_if_unchanged_statement"),
        (DB, "note_update_properties_statement"),
        (DB, "note_set_property_statement"),
        (DB, "note_soft_delete_statement"),
        (DB, "execute_filtered_note_property_patch"),
        (EVENTS, "claim_pending_event"),
        (EVENTS, "mark_dispatch_invoking"),
        (EVENTS, "renew_dispatch_lease"),
        (EVENTS, "persist_dispatch_outcome"),
        (EVENTS, "requeue_legacy_claim"),
        (EVENTS, "finalize_corrupt_receipt"),
        (EVENTS, "finalize_firing_event"),
        (GTD, "gtd_transition_statement"),
        (SCHEDULE, "cancel_pending_event"),
        (CURATION, "merge_note_sql"),
        (CREATE, "prepare_note_create"),
        // This feature can compile outside tests; keep its zero-row writer visible.
        (FAULT, "injected_failure_statement"),
    ]
    .into_iter()
    .map(|(file, owner)| (file.to_owned(), owner.to_owned()))
    .collect();
    assert!(
        !found.is_empty(),
        "production writer enumeration must not be vacuous"
    );
    assert_eq!(found.keys().cloned().collect::<BTreeSet<_>>(), expected);
    found
}

fn fixture(kind: &str, properties: &str) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    khive_db::migrations::run_migrations(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO notes (id,namespace,kind,status,content,properties,created_at,updated_at) \
         VALUES (?1,'local',?2,'active','fixture',?3,100,100)",
        params![ID, kind, properties],
    )
    .unwrap();
    install_authorizer(&conn);
    conn
}

fn install_authorizer(conn: &Connection) {
    conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Update {
            table_name: "notes",
            column_name: "version",
        } if context.accessor != Some("bump_note_version") => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .unwrap();
}

fn version(conn: &Connection) -> i64 {
    conn.query_row("SELECT version FROM notes WHERE id=?1", [ID], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn note_version_production_writers_never_assign_version() {
    let conn = fixture("memory", "{}");
    for ((file, owner), sql) in census() {
        let concrete = if owner == "execute_filtered_note_property_patch" {
            sql.replace("{{}}", "{}")
                .replace("{p1}", "1")
                .replace("{p2}", "2")
                .replace("{p3}", "3")
                .replace("{p4}", "4")
                .replace("{where_clause}", "WHERE namespace='local'")
        } else {
            sql
        };
        conn.prepare(&concrete)
            .unwrap_or_else(|e| panic!("{file}::{owner}: {e}\n{concrete}"));
    }
    assert!(conn
        .prepare("UPDATE notes SET version=version+1 WHERE id=?1")
        .is_err());
    assert!(conn
        .prepare("UPDATE notes SET \"version\"=7 WHERE id=?1")
        .is_err());
    assert!(conn
        .prepare("UPDATE notes SET content=content WHERE version=?1")
        .is_ok());
    assert!(conn
        .prepare("UPDATE notes SET properties=json_set(properties,'$.version',7)")
        .is_ok());
}

#[test]
fn note_version_one_real_writer_per_file_advances_exactly_once() {
    let writers = census();
    let cases = [
        (DB, "note_update_properties_statement", "memory", "{}"),
        (
            EVENTS,
            "requeue_legacy_claim",
            "scheduled_event",
            r#"{"status":"firing"}"#,
        ),
        (
            GTD,
            "gtd_transition_statement",
            "task",
            r#"{"status":"inbox"}"#,
        ),
        (
            SCHEDULE,
            "cancel_pending_event",
            "scheduled_event",
            r#"{"status":"pending"}"#,
        ),
        (CURATION, "merge_note_sql", "memory", "{}"),
        (CREATE, "prepare_note_create", "memory", "{}"),
        (FAULT, "injected_failure_statement", "memory", "{}"),
    ];
    assert_eq!(
        writers
            .keys()
            .map(|(file, _)| file.as_str())
            .collect::<BTreeSet<_>>(),
        cases.iter().map(|(file, ..)| *file).collect()
    );
    for (file, owner, kind, properties) in cases {
        let conn = fixture(kind, properties);
        let sql = &writers[&(file.to_owned(), owner.to_owned())];
        assert_eq!(version(&conn), 1);
        let changed = match file {
            DB => conn.execute(sql, params![r#"{"checked":true}"#, 200_i64, ID]),
            EVENTS => conn.execute(sql, params![200_i64, ID, "local", 100_i64, properties]),
            GTD => conn.execute(
                sql,
                params![
                    r#"{"status":"active"}"#,
                    200_i64,
                    ID,
                    100_i64,
                    rusqlite::types::Null,
                    "inbox"
                ],
            ),
            SCHEDULE => conn.execute(sql, params!["2026-09-09T00:00:00Z", 200_i64, ID, "local"]),
            CURATION => conn.execute(sql, params![200_i64, "local", ID]),
            CREATE => conn.execute(sql, params!["census/key", ID, "local", "memory"]),
            FAULT => conn.execute(sql, []),
            _ => unreachable!(),
        }
        .unwrap_or_else(|e| panic!("{file}::{owner}: {e}"));
        let expected = usize::from(file != FAULT);
        assert_eq!(changed, expected, "{file}::{owner}");
        assert_eq!(version(&conn), 1 + expected as i64, "{file}::{owner}");
    }
}

#[test]
fn note_version_scanner_controls() {
    let mut scanner = Scanner::default();
    scanner.visit_file(
        &syn::parse_file(
            r#"
        #[cfg(test)] mod tests { fn hidden() { call("UPDATE notes SET version=9"); } }
        fn live() { format!(r"UPDATE OR IGNORE notes SET content=?1 WHERE version=?2"); }
        fn more() { vec!["UPDATE notes SET properties='{}'"]; }
        fn joined() { concat!("UPDATE ", "main.notes SET content=content"); }
        fn commented() { call("UPDATE /* state */ main.\"notes\" SET content=content"); }
        #[cfg(any(test, feature="fault-injection"))]
        fn feature() { call("UPDATE notes SET content=content WHERE 1=0"); }
    "#,
        )
        .unwrap(),
    );
    assert_eq!(
        scanner
            .statements
            .iter()
            .map(|(owner, _)| owner.as_str())
            .collect::<Vec<_>>(),
        ["live", "more", "joined", "commented", "feature"]
    );
    for source in [
        r#"fn bad() { format!("UPDATE {} SET version=7", "notes"); }"#,
        r#"fn bad() { call("INSERT OR IGNORE INTO notes (id,version) VALUES (1,7)"); }"#,
    ] {
        assert!(
            std::panic::catch_unwind(|| {
                Scanner::default().visit_file(&syn::parse_file(source).unwrap());
            })
            .is_err(),
            "scanner must reject {source}"
        );
    }
}

#[test]
fn note_version_sql_files_are_inventoried_and_trigger_is_the_only_exception() {
    let root = root();
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let sql = entry.unwrap().path().join("sql");
        if sql.is_dir() {
            files(&sql, "sql", &mut sources);
        }
    }
    let found: BTreeSet<_> = sources
        .into_iter()
        .filter(|path| note_writer(&std::fs::read_to_string(path).unwrap()))
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        found,
        [
            "khive-db/sql/005-unique-comm-external-id.sql",
            "khive-db/sql/030-note-versions.sql",
            "khive-db/sql/notes-ddl.sql"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );

    let historical = fixture("memory", "{}");
    historical
        .execute_batch(include_str!(
            "../../khive-db/sql/005-unique-comm-external-id.sql"
        ))
        .unwrap();
    assert_eq!(version(&historical), 1);
    for direct in [false, true] {
        let conn = Connection::open_in_memory().unwrap();
        if !direct {
            for migration in khive_db::migrations::MIGRATIONS
                .iter()
                .filter(|m| m.version < 30)
            {
                conn.execute_batch(migration.up).unwrap();
            }
        }
        install_authorizer(&conn);
        conn.execute_batch(if direct {
            include_str!("../../khive-db/sql/notes-ddl.sql")
        } else {
            include_str!("../../khive-db/sql/030-note-versions.sql")
        })
        .unwrap();
        conn.execute("INSERT INTO notes (id,namespace,kind,created_at,updated_at) VALUES (?1,'local','memory',1,1)", [ID]).unwrap();
        assert_eq!(version(&conn), 1);
        assert_eq!(
            conn.execute("UPDATE notes SET content=content WHERE id=?1", [ID])
                .unwrap(),
            1
        );
        assert_eq!(version(&conn), 2, "direct DDL: {direct}");
    }
}
