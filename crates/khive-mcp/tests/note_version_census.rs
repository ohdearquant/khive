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
const OPERATIONS: &str = "khive-runtime/src/operations.rs";
const MESSAGE: &str = "khive-runtime/src/keyed_message.rs";
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

fn starts_with_ci(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
    bytes.len() >= at + needle.len() && bytes[at..at + needle.len()].eq_ignore_ascii_case(needle)
}

/// The assignment list of an `UPDATE`: the text between the top-level `SET` and
/// the top-level `WHERE`, with parentheses and every quoting form respected.
///
/// Splitting at the first ` WHERE ` would end the list at a subquery's own
/// predicate and stop looking exactly where a later assignment could still be
/// hiding, so this walks the statement instead.
///
/// All four of SQLite's quoting forms are tracked, not just `\'`, and the reason
/// is that missing one shortens the list, which is the UNSAFE direction. With
/// only `\'` handled, `UPDATE {} SET "col WHERE x" = ?1, version = ?2 WHERE id = ?3`
/// ends its list at `"col`, never reaches the `version` assignment, and is
/// admitted. Measured against this function before the other three were added,
/// with `\`` and `[...]` behaving the same way.
fn assignment_list(sql: &str) -> Option<&str> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    // The delimiter that would close the span currently open, so that `[` can be
    // closed by `]` rather than by itself. `None` means not inside one.
    let mut closes: Option<u8> = None;
    let mut start: Option<usize> = None;
    // Where the list ends, recorded rather than returned, so the scan carries on
    // to the end of the literal looking for a statement separator.
    let mut end: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(end) = closes {
            if c == end {
                closes = None;
            }
            i += 1;
            continue;
        }
        // Comments are skipped for the same reason quoted spans are, and they are
        // the other half of one class: ANY run of text that can contain the
        // characters ` WHERE ` without being a predicate will end the assignment
        // list early if it is not skipped, and a shorter list is the direction
        // that ADMITS. Measured on this scanner: with comments unhandled,
        // `UPDATE {} SET a = ?1 /* WHERE */, version = ?2 WHERE id = ?3` produced
        // the list `a = ?1 /*` and was admitted, and the `--` form did the same.
        if c == b'-' && starts_with_ci(bytes, i, b"--") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && starts_with_ci(bytes, i, b"/*") {
            i += 2;
            while i < bytes.len() && !starts_with_ci(bytes, i, b"*/") {
                i += 1;
            }
            // An unterminated comment runs to the end of the literal, which
            // leaves `start` set and no top-level WHERE found, so the list
            // becomes everything after SET. That is the refusing direction.
            i = (i + 2).min(bytes.len());
            continue;
        }
        match c {
            b'\'' => closes = Some(b'\''),
            b'"' => closes = Some(b'"'),
            b'`' => closes = Some(b'`'),
            b'[' => closes = Some(b']'),
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            // A `;` outside every quoted span and comment separates STATEMENTS, and
            // this predicate reasons about one. A second statement assigns
            // whatever it likes while the first supplies a `WHERE` that ends the
            // list before it: measured on this scanner,
            // `UPDATE {} SET a = 1; SELECT 1 WHERE 1=1; UPDATE {} SET version = 2
            // WHERE id = 1` yielded the list `a = 1; SELECT 1`, which carries no
            // `VERSION`, and was admitted. Refusing the whole literal is the
            // direction that costs nothing: a reader of one statement has no
            // business ruling on two.
            //
            // A `;` closing a single statement is not that, so it ends the scan
            // rather than refusing; otherwise the ordinary trailing semicolon
            // would refuse every statement that carries one.
            b';' => {
                if bytes[i + 1..].iter().all(u8::is_ascii_whitespace) {
                    break;
                }
                return None;
            }
            _ => {
                if depth == 0 {
                    if start.is_none() && starts_with_ci(bytes, i, b" SET ") {
                        start = Some(i + 5);
                        i += 5;
                        continue;
                    }
                    if start.is_some() && end.is_none() && starts_with_ci(bytes, i, b" WHERE ") {
                        end = Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    start.map(|from| &sql[from..end.unwrap_or(sql.len())])
}

/// Whether a statement whose table name is interpolated can still be ruled out as
/// a writer of `notes.version`.
///
/// The census cannot resolve `UPDATE {} SET ...` to a table, and for that reason
/// it refused every dynamic target outright. That refuses on the wrong axis. The
/// invariant is that no production writer assigns `version`, and the assignment
/// list decides it on its own: if that list is static and names no `version`
/// column, then no table the interpolation can resolve to has its version
/// assigned here. A `WHERE` clause assigns nothing, so it may stay dynamic.
fn assignments_rule_out_version(sql: &str) -> bool {
    match assignment_list(sql) {
        // `VERSION` is searched for as a case-insensitive substring of the whole
        // list, never as a token. `json_set(properties, '$.version', ?2)` assigns
        // the field without ever spelling it as a bare word, so a tokenizer that
        // respected quoting would admit exactly the statement this exists to
        // refuse. Over-refusal is the safe direction, so a column merely
        // containing the letters (`versioned_at`) refuses too; narrowing that is
        // a deliberate later change, not something to be clever about here.
        Some(list) => !list.contains('{') && !list.to_ascii_uppercase().contains("VERSION"),
        None => false,
    }
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
                !target.contains('{') || assignments_rule_out_version(&sql),
                "dynamic UPDATE target with an uninspectable assignment list in {}: {sql}",
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
        (OPERATIONS, "restore_note"),
        // Keyed message pairs stamp the caller key onto the outbound note in a
        // second statement, so a freshly created pair settles at version 2. The
        // writer never assigns the column itself; the trigger does.
        (MESSAGE, "create_keyed_message_pair"),
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

fn deleted_fixture(kind: &str, properties: &str) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    khive_db::migrations::run_migrations(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO notes (id,namespace,kind,status,content,properties,created_at,updated_at,deleted_at) \
         VALUES (?1,'local',?2,'deleted','fixture',?3,100,100,150)",
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
        } else if owner == "restore_note" {
            sql.replace("{key_clause}", "")
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
        (OPERATIONS, "restore_note", "memory", "{}"),
        // The keyed pair stamps the caller key onto an already-inserted
        // outbound note, so the fixture is a keyless message row.
        (MESSAGE, "create_keyed_message_pair", "message", "{}"),
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
        let conn = if file == OPERATIONS {
            deleted_fixture(kind, properties)
        } else {
            fixture(kind, properties)
        };
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
            OPERATIONS => conn.execute(
                &sql.replace("{key_clause}", ""),
                params!["active", 200_i64, ID, "local", "memory"],
            ),
            MESSAGE => conn.execute(sql, params!["census/key", ID, "local"]),
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
            "khive-db/sql/031-note-versions.sql",
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
            include_str!("../../khive-db/sql/031-note-versions.sql")
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

/// The relaxation that admits an interpolated table name, stated as the set of
/// shapes it admits and the set it still refuses.
///
/// This arm exists because the relaxation's only new beneficiary is the namespace
/// mover, written in the same change, so "it still catches what it was for" is a
/// claim that has to be executed rather than asserted in prose.
#[test]
fn a_dynamic_update_target_is_admitted_only_on_a_static_version_free_assignment_list() {
    // The shapes the namespace-move primitive needs. It is schema-derived over
    // every table carrying a namespace column and SQLite cannot bind a table
    // name, so an interpolated target is not avoidable there.
    assert!(assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2 WHERE namespace = ?1 AND kind = ?3"
    ));
    assert!(assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2 WHERE namespace = ?1"
    ));
    // A dynamic predicate is still fine: a WHERE clause assigns nothing.
    assert!(assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2 WHERE namespace = ?1 AND subject_id IN ({selector})"
    ));

    // Still refused: the assignment list names the column this census protects.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET version = ?2 WHERE id = ?1"
    ));
    // Still refused: the assignment list is itself interpolated, so nothing about
    // it can be read at all.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET {} WHERE id = ?1"
    ));
    // Still refused, and this is the one a depth-blind split at the first
    // " WHERE " would have admitted: the version assignment sits after a
    // subquery's own predicate.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET a = (SELECT 1 FROM t WHERE x = ?1), version = ?2 WHERE id = ?3"
    ));
    // Still refused: the version assignment is not the first one. A rule reading
    // only the assignment nearest to SET would admit this.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2, version = ?3 WHERE id = ?1"
    ));
    // Still refused, and this is the shape that makes the substring search
    // load-bearing: the column is assigned through a json path, so `VERSION`
    // never appears as a word and a quoting-aware tokenizer admits it.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET properties = json_set(properties,'$.version',?2) WHERE id = ?1"
    ));
    // Still refused: no SET at all reads as unknown, not as safe.
    assert!(!assignments_rule_out_version("UPDATE {} WHERE id = ?1"));

    // Admitted with no WHERE at all: the list then runs to the end of the
    // literal, which is the branch `assignment_list` takes when it finds no
    // top-level predicate.
    assert!(assignments_rule_out_version("UPDATE {} SET namespace = ?2"));
}

/// A quoted identifier cannot be used to hide the rest of the assignment list.
///
/// SQLite quotes identifiers four ways, and the scanner originally tracked only
/// `'`. That is not a cosmetic gap: an unhandled quote lets a ` WHERE ` INSIDE an
/// identifier end the assignment list early, so everything assigned after it is
/// never inspected and the statement is admitted. Shortening the list is the
/// unsafe direction, which is why each form gets an arm rather than a comment.
#[test]
fn a_where_inside_a_quoted_identifier_does_not_end_the_assignment_list() {
    for opened in [
        r#"UPDATE {} SET "col WHERE x" = ?1, version = ?2 WHERE id = ?3"#,
        "UPDATE {} SET `col WHERE x` = ?1, version = ?2 WHERE id = ?3",
        r#"UPDATE {} SET [col WHERE x] = ?1, version = ?2 WHERE id = ?3"#,
        r#"UPDATE {} SET a = ' WHERE ', version = ?2 WHERE id = ?3"#,
    ] {
        assert!(
            !assignments_rule_out_version(opened),
            "a WHERE inside a quoted span must not end the list: {opened}"
        );
    }

    // The control, in the same arm: the same statements without the version
    // assignment are still admitted, so the arm above is not passing merely
    // because every quoted identifier now refuses.
    for benign in [
        r#"UPDATE {} SET "col WHERE x" = ?1 WHERE id = ?3"#,
        "UPDATE {} SET `col WHERE x` = ?1 WHERE id = ?3",
    ] {
        assert!(
            assignments_rule_out_version(benign),
            "a quoted identifier is not itself a reason to refuse: {benign}"
        );
    }
}

/// A comment cannot be used to hide the rest of the assignment list either.
///
/// Same class as the quoted identifier above and the same unsafe direction: a
/// comment holding the text ` WHERE ` ends the list early, so everything assigned
/// after the comment is never inspected. Both comment forms get an arm because
/// they terminate differently, and an unterminated block comment gets one because
/// its fallback has to be the refusing direction rather than a panic or a
/// truncated read.
#[test]
fn a_where_inside_a_comment_does_not_end_the_assignment_list() {
    for hidden in [
        "UPDATE {} SET a = ?1 /* WHERE */, version = ?2 WHERE id = ?3",
        "UPDATE {} SET a = ?1, -- WHERE \n version = ?2 WHERE id = ?3",
        "UPDATE {} SET a = ?1 /* WHERE and never closed, version = ?2",
    ] {
        assert!(
            !assignments_rule_out_version(hidden),
            "a WHERE inside a comment must not end the list: {hidden}"
        );
    }

    // The control: a comment that hides nothing is not itself a reason to refuse,
    // so the arm above cannot be passing merely because comments now refuse.
    assert!(assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2 /* the move itself, nothing hidden */ WHERE id = ?1"
    ));

    // What that costs, executed rather than described. The comment's own text
    // sits inside the assignment list, so a comment that merely spells the word
    // refuses. The first draft of the control above read
    // `/* the move, not a version write */` and failed right here. It is the same
    // over-refusal `assignments_rule_out_version` already takes for a column named
    // `versioned_at`, and it is recorded rather than removed: stripping comment
    // text before the substring check would hand the word a place to sit where
    // nothing looks at it, and a comment is not somewhere a caller needs an
    // admission from.
    assert!(!assignments_rule_out_version(
        "UPDATE {} SET namespace = ?2 /* not a version write */ WHERE id = ?1"
    ));
}

/// Two statements in one literal are two statements, and this reads one.
///
/// Third of the same class as the quoted identifier and the comment, and the one
/// that does not fit their shape: here the text ending the list early is a real
/// `WHERE`, belonging to a real predicate, of a different statement. Whatever the
/// second statement assigns is outside everything the predicate looks at, so the
/// refusal is on the LITERAL rather than on the list.
///
/// Both placements get a case because they fail differently: a separator inside
/// the list leaves a `;` in the text a list-scoped rule could still see, and one
/// after the list leaves nothing there at all.
#[test]
fn a_second_statement_in_one_literal_is_not_read_and_so_is_refused() {
    for hidden in [
        "UPDATE {} SET a = 1; SELECT 1 WHERE 1=1; UPDATE {} SET version = 2 WHERE id = 1",
        "UPDATE {} SET a = 1 WHERE id = 1; UPDATE notes SET version = 2 WHERE id = 1",
    ] {
        assert!(
            !assignments_rule_out_version(hidden),
            "a second statement is never read, so a literal holding one is refused: {hidden}"
        );
    }

    // The controls, and a rule keyed on the character alone would refuse both: a
    // single statement written with its terminator, and a semicolon inside a
    // quoted value, where it is data rather than a separator.
    for benign in [
        "UPDATE {} SET namespace = ?2 WHERE id = ?1;",
        "UPDATE {} SET namespace = ?2 WHERE note = ';'",
    ] {
        assert!(
            assignments_rule_out_version(benign),
            "one statement is still one statement: {benign}"
        );
    }
}

/// What would have to be true for the predicate to be wrong, executed rather
/// than described.
///
/// The predicate is a conjunction, and a conjunction invites the reading that one
/// half is redundant. These are the two collapses, each run against the arms
/// above:
///
/// - Widening the brace check from the assignment list to the whole literal
///   refuses the mover's real statement, because its `WHERE` carries an
///   interpolated selector. That is the reason the check is scoped to the list.
/// - Dropping the brace check and keeping only `VERSION` admits `SET {}`, where
///   nothing at all can be read. That is the reason the two halves are separate
///   rather than one.
#[test]
fn neither_half_of_the_predicate_is_redundant() {
    // The mover's real statement, the one the relaxation exists for.
    let movers = "UPDATE {} SET namespace = ?2 WHERE namespace = ?1 AND subject_id IN ({selector})";
    // An interpolated assignment list, which no rule can inspect.
    let opaque = "UPDATE {} SET {} WHERE id = ?1";

    // The predicate as written: admits the first, refuses the second.
    assert!(assignments_rule_out_version(movers));
    assert!(!assignments_rule_out_version(opaque));

    // Falsifier 1 — brace check widened to the whole literal. This is the
    // version that reddens the arm the change is for.
    let no_brace_anywhere = |sql: &str| !sql.contains('{');
    assert!(
        !no_brace_anywhere(movers),
        "if this ever admits the mover's statement, the assignment-list scoping \
         has stopped being load-bearing and this whole relaxation is unmotivated"
    );

    // Falsifier 2 — the VERSION half alone, with the brace check dropped. It
    // reddens nothing new, and admits the statement nobody can read.
    let version_only = |sql: &str| match assignment_list(sql) {
        Some(list) => !list.to_ascii_uppercase().contains("VERSION"),
        None => false,
    };
    assert!(
        version_only(opaque),
        "the VERSION half cannot see an interpolated assignment list, which is \
         why the brace check is a separate conjunct rather than a special case"
    );
    assert!(version_only(movers), "and it agrees on everything else");
}

/// The scanner's own refusal, driven through the real visitor rather than the
/// predicate, so the wiring is covered too.
#[test]
#[should_panic(expected = "uninspectable assignment list")]
fn a_dynamic_update_assigning_version_still_stops_the_scanner() {
    let source = r#"
        fn writer() -> String {
            format!("UPDATE {} SET version = ?2 WHERE id = ?1", table)
        }
    "#;
    let mut scanner = Scanner::default();
    scanner.visit_file(&syn::parse_file(source).unwrap());
}

/// The positive half of the pair. Without it the arm above passes on a scanner
/// that panics at everything.
#[test]
fn a_dynamic_update_setting_only_namespace_passes_the_scanner() {
    let source = r#"
        fn writer() -> String {
            format!("UPDATE {} SET namespace = ?2 WHERE namespace = ?1", table)
        }
    "#;
    let mut scanner = Scanner::default();
    scanner.visit_file(&syn::parse_file(source).unwrap());
}
