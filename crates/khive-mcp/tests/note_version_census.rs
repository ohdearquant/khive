use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{params, Connection};
use syn::parse::Parser;
use syn::visit::Visit;

#[path = "../../khive-runtime/tests/support/static_sql_source.rs"]
mod static_sql_source;
use static_sql_source::{CanonicalBindings, StaticSqlSources};

const LEGACY_SQL_WRITERS: &[&str] = &[
    "khive-db/sql/005-unique-comm-external-id.sql",
    "khive-db/sql/031-note-versions.sql",
    "khive-db/sql/notes-ddl.sql",
];
const APPLICATION_SQL_WRITERS: &[(&str, &str, &str)] = &[
    (
        "khive-pack-comm/sql/quarantine_duplicate_retention_repair.sql",
        COMM_INGEST,
        "repair_duplicate_quarantine",
    ),
    (
        "khive-pack-gtd/sql/task-transition-update.sql",
        GTD,
        "gtd_transition_statement",
    ),
    (
        "khive-pack-gtd/sql/task-repair-update.sql",
        GTD_REPAIR,
        "checked_update_sql",
    ),
];

const DB: &str = "khive-db/src/stores/note.rs";
const MIGRATIONS: &str = "khive-db/src/migrations.rs";
const RECLAIM: &str = "khive-mcp/src/pending_events/reclaim.rs";
const RECEIPTS: &str = "khive-mcp/src/pending_events/receipt.rs";
const GTD: &str = "khive-pack-gtd/src/handlers.rs";
const GTD_REPAIR: &str = "khive-pack-gtd/src/repair.rs";
const SCHEDULE: &str = "khive-pack-schedule/src/handlers.rs";
const CURATION: &str = "khive-runtime/src/curation/merge_sql.rs";
const CREATE: &str = "khive-runtime/src/note_create.rs";
const OPERATIONS: &str = "khive-runtime/src/operations.rs";
const MESSAGE: &str = "khive-runtime/src/keyed_message.rs";
const FAULT: &str = "khive-runtime/src/atomic_message.rs";
const COMM: &str = "khive-pack-comm/src/handlers.rs";
const COMM_INGEST: &str = "khive-pack-comm/src/handlers/ingest.rs";
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

#[derive(Debug)]
enum InitializerToken {
    Word(String),
    Quoted(String),
    Symbol(char),
}

impl InitializerToken {
    fn keyword(&self, expected: &str) -> bool {
        matches!(self, Self::Word(word) if word.eq_ignore_ascii_case(expected))
    }

    fn identifier(&self) -> Option<&str> {
        match self {
            Self::Word(word) | Self::Quoted(word) => Some(word),
            Self::Symbol(_) => None,
        }
    }

    fn symbol(&self, expected: char) -> bool {
        matches!(self, Self::Symbol(symbol) if *symbol == expected)
    }
}

fn initializer_tokens(sql: &str) -> Vec<InitializerToken> {
    let mut input = sql.chars().peekable();
    let mut tokens = Vec::new();
    let identifier_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '$') || !c.is_ascii();
    while let Some(c) = input.next() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\u{feff}') {
            continue;
        }
        if c == '-' && input.peek() == Some(&'-') {
            for c in input.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if c == '/' && input.peek() == Some(&'*') {
            input.next();
            // SQLite block comments end at the first closing pair; they do not nest.
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
            // Single quotes may denote identifiers in SQLite identifier positions,
            // but no quoted token may start an INSERT or stand in for INTO.
            tokens.push(InitializerToken::Quoted(quoted));
        } else if identifier_char(c) {
            let mut word = String::from(c);
            while input.peek().is_some_and(|c| identifier_char(*c)) {
                word.push(input.next().unwrap());
            }
            tokens.push(InitializerToken::Word(word));
        } else {
            tokens.push(InitializerToken::Symbol(c));
        }
    }
    tokens
}

fn initializer_columns_at(tokens: &[InitializerToken], start: usize) -> Option<usize> {
    let mut at = start + 1;
    if tokens[start].keyword("INSERT") {
        if tokens.get(at).is_some_and(|token| token.keyword("OR")) {
            at += 1;
            if !tokens.get(at).is_some_and(|token| {
                ["ROLLBACK", "ABORT", "REPLACE", "FAIL", "IGNORE"]
                    .iter()
                    .any(|action| token.keyword(action))
            }) {
                return None;
            }
            at += 1;
        }
    } else if !tokens[start].keyword("REPLACE") {
        return None;
    }
    if !tokens.get(at).is_some_and(|token| token.keyword("INTO")) {
        return None;
    }
    at += 1;
    let first = tokens.get(at)?.identifier()?;
    at += 1;
    let table = if tokens.get(at).is_some_and(|token| token.symbol('.')) {
        if !first.eq_ignore_ascii_case("MAIN") {
            return None;
        }
        at += 1;
        let table = tokens.get(at)?.identifier()?;
        at += 1;
        table
    } else {
        first
    };
    if !table.eq_ignore_ascii_case("NOTES") {
        return None;
    }
    if tokens.get(at).is_some_and(|token| token.keyword("AS")) {
        at += 1;
        tokens.get(at)?.identifier()?;
        at += 1;
    }
    tokens
        .get(at)
        .is_some_and(|token| token.symbol('('))
        .then_some(at + 1)
}

fn assert_note_version_implicit(sql: &str, owner: &str) {
    let tokens = initializer_tokens(sql);
    for start in 0..tokens.len() {
        let Some(mut column) = initializer_columns_at(&tokens, start) else {
            continue;
        };
        while let Some(name) = tokens.get(column).and_then(InitializerToken::identifier) {
            assert!(
                !name.eq_ignore_ascii_case("VERSION"),
                "{owner} explicitly initializes note.version"
            );
            if !tokens
                .get(column + 1)
                .is_some_and(|token| token.symbol(','))
            {
                break;
            }
            column += 2;
        }
    }
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
            || (a.path().segments.len() == 2
                && a.path().segments[0].ident == "tokio"
                && a.path().segments[1].ident == "test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

#[derive(Default)]
struct Scanner<'a> {
    owner: String,
    statements: Vec<(String, String)>,
    source_path: String,
    sql_sources: Option<&'a StaticSqlSources>,
    loader_bindings: CanonicalBindings,
    loaded_writers: Vec<(String, String)>,
    markerless_calls: BTreeSet<(String, String)>,
    recipient_stores: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
            let stores = std::mem::take(&mut self.recipient_stores);
            syn::visit::visit_item_fn(self, item);
            self.recipient_stores = stores;
            self.owner = old;
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
            let stores = std::mem::take(&mut self.recipient_stores);
            syn::visit::visit_impl_item_fn(self, item);
            self.recipient_stores = stores;
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
            let nested = static_sql_source::module_bindings(item, &self.loader_bindings, test_only);
            let old = std::mem::replace(&mut self.loader_bindings, nested);
            syn::visit::visit_item_mod(self, item);
            self.loader_bindings = old;
            self.loader_bindings.observe_module(item);
        }
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if !test_only(&item.attrs) {
            self.loader_bindings.observe_macro(item);
            syn::visit::visit_item_macro(self, item);
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let nested = static_sql_source::block_bindings(block, &self.loader_bindings, test_only);
        let old = std::mem::replace(&mut self.loader_bindings, nested);
        syn::visit::visit_block(self, block);
        self.loader_bindings = old;
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_impl(self, item);
        }
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let (syn::Pat::Ident(binding), Some(init)) = (&local.pat, &local.init) {
            if let syn::Expr::Call(call) = init.expr.as_ref() {
                if let syn::Expr::Path(path) = call.func.as_ref() {
                    let parts: Vec<_> = path
                        .path
                        .segments
                        .iter()
                        .map(|part| part.ident.to_string())
                        .collect();
                    if parts.ends_with(&["RecipientTransportStore".into(), "new".into()]) {
                        self.recipient_stores.insert(binding.ident.to_string());
                    }
                }
            }
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        if markerless_route(&method) {
            self.markerless_calls.insert((self.owner.clone(), method));
        } else if method == "commit" {
            let known_receiver = matches!(call.receiver.as_ref(), syn::Expr::Path(path)
                if path
                    .path
                    .get_ident()
                    .is_some_and(|name| self.recipient_stores.contains(&name.to_string()))
            );
            let recipient_argument = call.args.iter().any(|arg| {
                matches!(arg, syn::Expr::Struct(value)
                if value.path.segments.last().is_some_and(|part| part.ident == "RecipientCommit"))
            });
            if known_receiver || recipient_argument {
                self.markerless_calls
                    .insert((self.owner.clone(), "RecipientTransportStore::commit".into()));
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            if let Some(last) = path.path.segments.last() {
                let name = last.ident.to_string();
                if markerless_route(&name) {
                    self.markerless_calls.insert((self.owner.clone(), name));
                } else if name == "commit"
                    && path
                        .path
                        .segments
                        .iter()
                        .any(|part| part.ident == "RecipientTransportStore")
                {
                    self.markerless_calls
                        .insert((self.owner.clone(), "RecipientTransportStore::commit".into()));
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_block(&mut self, item: &'ast syn::ExprBlock) {
        if !test_only(&item.attrs) {
            syn::visit::visit_expr_block(self, item);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let empty = StaticSqlSources::new();
        if let Some(sql) = static_sql_source::resolve_static_sql(
            &self.source_path,
            mac,
            self.sql_sources.unwrap_or(&empty),
            &self.loader_bindings,
        )
        .unwrap_or_else(|error| panic!("{error}"))
        {
            if !LEGACY_SQL_WRITERS.contains(&sql.asset_path.as_str()) {
                if note_writer(&sql.produced_text) {
                    self.loaded_writers
                        .push((sql.asset_path, self.owner.clone()));
                }
                self.process_sql(sql.produced_text);
            }
            return;
        }
        if mac.path.is_ident("vec") {
            let expression = syn::parse_str::<syn::Expr>(&format!("[{}]", mac.tokens))
                .unwrap_or_else(|error| panic!("{}: invalid vec! body: {error}", self.source_path));
            self.visit_expr(&expression);
            return;
        }
        assert!(
            !static_sql_source::opaque_sql_loader(mac),
            "{}: uninspectable nested SQL loader",
            self.source_path
        );
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
        self.process_sql(literal.value());
    }
}

impl Scanner<'_> {
    fn process_sql(&mut self, sql: String) {
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
        assert_note_version_implicit(&sql, &self.owner);
        if note_writer(&sql) {
            assert!(!self.owner.is_empty(), "note writer needs a named owner");
            self.statements.push((self.owner.clone(), sql));
        }
    }
}

fn markerless_route(name: &str) -> bool {
    matches!(
        name,
        "upsert_note"
            | "insert_note_if_absent"
            | "try_insert_note"
            | "try_insert_note_with_attachments"
            | "upsert_notes"
            | "batch_upsert_notes"
            | "note_upsert_statement"
            | "note_insert_if_absent_statement"
            | "note_insert_keyed_statement"
    )
}

#[derive(Clone, Copy)]
enum MarkerDisposition {
    StorageConstructor,
    UnkeyedNote,
    FixedNonMemoryKind,
    RuntimeReceiptUnit,
}

// ADR-144 A4: only RuntimeReceiptUnit adds modern atomically; constructors and
// forwarding wrappers do not infer provenance from the current binary.
const MARKERLESS_CALLERS: &[(&str, &str, &str, MarkerDisposition)] = &[
    (
        DB,
        "note_insert_if_absent_statement",
        "note_upsert_statement",
        MarkerDisposition::StorageConstructor,
    ),
    (
        DB,
        "note_insert_keyed_statement",
        "note_upsert_statement",
        MarkerDisposition::StorageConstructor,
    ),
    (
        DB,
        "upsert_note",
        "note_upsert_statement",
        MarkerDisposition::StorageConstructor,
    ),
    (
        DB,
        "insert_note_if_absent",
        "note_insert_if_absent_statement",
        MarkerDisposition::StorageConstructor,
    ),
    (
        DB,
        "try_insert_note",
        "try_insert_note_with_attachments",
        MarkerDisposition::StorageConstructor,
    ),
    (
        DB,
        "upsert_notes",
        "batch_upsert_notes",
        MarkerDisposition::StorageConstructor,
    ),
    (
        "khive-runtime/src/note_store_guard.rs",
        "upsert_note",
        "upsert_note",
        MarkerDisposition::StorageConstructor,
    ),
    (
        "khive-runtime/src/note_store_guard.rs",
        "insert_note_if_absent",
        "insert_note_if_absent",
        MarkerDisposition::StorageConstructor,
    ),
    (
        "khive-runtime/src/note_store_guard.rs",
        "upsert_notes",
        "upsert_notes",
        MarkerDisposition::StorageConstructor,
    ),
    (
        "khive-runtime/src/note_store_guard.rs",
        "try_insert_note",
        "try_insert_note",
        MarkerDisposition::StorageConstructor,
    ),
    (
        OPERATIONS,
        "create_note_inner",
        "upsert_note",
        MarkerDisposition::UnkeyedNote,
    ),
    (
        OPERATIONS,
        "try_create_note_impl",
        "try_insert_note",
        MarkerDisposition::UnkeyedNote,
    ),
    (
        OPERATIONS,
        "try_create_note_impl",
        "try_insert_note_with_attachments",
        MarkerDisposition::UnkeyedNote,
    ),
    (
        COMM,
        "handle_heartbeat",
        "insert_note_if_absent",
        MarkerDisposition::FixedNonMemoryKind,
    ),
    (
        "kkernel/src/code_ingest.rs",
        "persist_ingest_note",
        "upsert_note",
        MarkerDisposition::FixedNonMemoryKind,
    ),
    (
        "khive-runtime/src/atomic_prepare/add_update.rs",
        "prepare_add_note",
        "note_upsert_statement",
        MarkerDisposition::UnkeyedNote,
    ),
    (
        MESSAGE,
        "create_keyed_message_pair_with_attachments",
        "note_insert_if_absent_statement",
        MarkerDisposition::FixedNonMemoryKind,
    ),
    (
        FAULT,
        "prepare_atomic_note_requests",
        "note_insert_keyed_statement",
        MarkerDisposition::RuntimeReceiptUnit,
    ),
    (
        FAULT,
        "prepare_atomic_note_requests",
        "note_upsert_statement",
        MarkerDisposition::RuntimeReceiptUnit,
    ),
    (
        CREATE,
        "prepare_note_create",
        "note_insert_if_absent_statement",
        MarkerDisposition::RuntimeReceiptUnit,
    ),
    (
        "khive-runtime/src/comm_recipient.rs",
        "ingest_verified_recipient",
        "RecipientTransportStore::commit",
        MarkerDisposition::FixedNonMemoryKind,
    ),
];

fn assert_markerless_callers(
    found: &MarkerlessCalls,
    inventory: &[(&str, &str, &str, MarkerDisposition)],
) {
    assert!(!found.is_empty(), "markerless note-route coverage is empty");
    let expected = inventory
        .iter()
        .map(|(path, owner, route, _)| (path.to_string(), owner.to_string(), route.to_string()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        expected.len(),
        inventory.len(),
        "duplicate markerless caller disposition"
    );
    assert_eq!(
        *found, expected,
        "every markerless note-route caller needs an explicit disposition"
    );
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
        } else if path.extension().is_some_and(|e| e == extension) && {
            // Test-only modules are `<name>_tests.rs` or a directory module's `tests.rs`.
            let stem = path.file_stem().unwrap().to_string_lossy();
            !stem.ends_with("_tests") && stem != "tests"
        } {
            output.push(path);
        }
    }
}

fn live_sql_sources() -> StaticSqlSources {
    let root = root();
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let sql = entry.unwrap().path().join("sql");
        if sql.is_dir() {
            files(&sql, "sql", &mut paths);
        }
    }
    paths
        .into_iter()
        .map(|path| {
            (
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                std::fs::read_to_string(path).unwrap(),
            )
        })
        .collect()
}

type NamedSqlWriters = BTreeMap<(String, String), String>;
type LoadedSqlWriters = BTreeSet<(String, String, String)>;

fn scan_sources_with_sql(
    sources: &[(String, String)],
    sql_sources: &StaticSqlSources,
) -> (NamedSqlWriters, LoadedSqlWriters) {
    let (writers, loaded, _) = scan_sources_with_routes(sources, sql_sources);
    (writers, loaded)
}

type MarkerlessCalls = BTreeSet<(String, String, String)>;

fn scan_sources_with_routes(
    sources: &[(String, String)],
    sql_sources: &StaticSqlSources,
) -> (NamedSqlWriters, LoadedSqlWriters, MarkerlessCalls) {
    let parsed = sources
        .iter()
        .map(|(path, source)| (path.clone(), syn::parse_file(source).unwrap()))
        .collect::<Vec<_>>();
    let bindings = static_sql_source::canonical_bindings(&parsed, test_only);
    let mut found = BTreeMap::new();
    let mut loaded = BTreeSet::new();
    let mut markerless = BTreeSet::new();
    for (path, file) in &parsed {
        let mut scanner = Scanner {
            source_path: path.clone(),
            sql_sources: Some(sql_sources),
            loader_bindings: bindings[path].clone(),
            ..Scanner::default()
        };
        scanner.visit_file(file);
        for (owner, route) in scanner.markerless_calls {
            markerless.insert((path.clone(), owner, route));
        }
        for (asset, owner) in scanner.loaded_writers {
            loaded.insert((asset, path.clone(), owner));
        }
        for (owner, sql) in scanner.statements {
            let key = (path.clone(), owner);
            assert!(
                found.insert(key.clone(), sql).is_none(),
                "multiple writer templates: {key:?}"
            );
        }
    }
    (found, loaded, markerless)
}

fn assert_sql_ownership(
    sql_sources: &StaticSqlSources,
    loaded: &BTreeSet<(String, String, String)>,
) {
    let found = sql_sources
        .iter()
        .filter(|(_, text)| note_writer(text))
        .map(|(path, _)| path.as_str())
        .collect::<BTreeSet<_>>();
    let expected = LEGACY_SQL_WRITERS
        .iter()
        .copied()
        .chain(APPLICATION_SQL_WRITERS.iter().map(|(asset, _, _)| *asset))
        .collect();
    assert_eq!(found, expected, "SQL writer asset inventory");
    let expected = APPLICATION_SQL_WRITERS
        .iter()
        .map(|(asset, path, owner)| (asset.to_string(), path.to_string(), owner.to_string()))
        .collect();
    assert_eq!(*loaded, expected, "SQL writer asset-to-caller ownership");
}

fn census() -> BTreeMap<(String, String), String> {
    let root = root();
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            files(&src, "rs", &mut paths);
        }
    }
    let sources = paths
        .into_iter()
        .map(|path| {
            (
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                std::fs::read_to_string(path).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let sql_sources = live_sql_sources();
    let (found, loaded, markerless) = scan_sources_with_routes(&sources, &sql_sources);
    assert_markerless_callers(&markerless, MARKERLESS_CALLERS);
    assert_sql_ownership(&sql_sources, &loaded);
    let expected = [
        (DB, "NOTE_UPSERT_SQL"),
        (DB, "note_replace_if_unchanged_statement"),
        (DB, "note_metadata_replace_if_unchanged_statement"),
        (DB, "note_update_properties_statement"),
        (DB, "note_set_property_statement"),
        (DB, "note_soft_delete_statement"),
        (DB, "execute_filtered_note_property_patch"),
        (MIGRATIONS, "migrate_outbound_due_key"),
        (RECEIPTS, "claim_pending_event"),
        (RECEIPTS, "mark_dispatch_invoking"),
        (RECEIPTS, "renew_dispatch_lease"),
        (RECEIPTS, "persist_dispatch_outcome"),
        (RECLAIM, "requeue_legacy_claim"),
        (RECLAIM, "finalize_corrupt_receipt"),
        (RECLAIM, "finalize_firing_event"),
        (GTD, "gtd_transition_statement"),
        (GTD_REPAIR, "checked_update_sql"),
        (SCHEDULE, "cancel_pending_event"),
        (CURATION, "merge_note_sql"),
        (CREATE, "prepare_note_create"),
        (OPERATIONS, "restore_note"),
        // Keyed message pairs stamp the caller key onto the outbound note in a
        // second statement, so a freshly created pair settles at version 2. The
        // writer never assigns the column itself; the trigger does.
        (MESSAGE, "create_keyed_message_pair_with_attachments"),
        // A matching quarantine replay repairs legacy retention metadata in
        // one UPDATE. The note version trigger, not this writer, advances it.
        (COMM_INGEST, "repair_duplicate_quarantine"),
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
                .replace("{p5}", "5")
                .replace("{p6}", "6")
                .replace("{p7}", "7")
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
        (MIGRATIONS, "migrate_outbound_due_key", "memory", "{}"),
        (
            RECLAIM,
            "requeue_legacy_claim",
            "scheduled_event",
            r#"{"status":"firing"}"#,
        ),
        (
            RECEIPTS,
            "renew_dispatch_lease",
            "scheduled_event",
            r#"{"status":"firing","firing_at":100,"dispatch_receipt":{"invocation_id":"00000000-0000-4000-8000-000000000002","state":"invoking"}}"#,
        ),
        (
            GTD,
            "gtd_transition_statement",
            "task",
            r#"{"status":"inbox"}"#,
        ),
        (
            GTD_REPAIR,
            "checked_update_sql",
            "task",
            r#"{"status":"archived"}"#,
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
        (
            MESSAGE,
            "create_keyed_message_pair_with_attachments",
            "message",
            "{}",
        ),
        (
            COMM_INGEST,
            "repair_duplicate_quarantine",
            "message",
            r#"{"quarantined":true,"quarantine_content_ref":"census-ref","channel_kind":"email"}"#,
        ),
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
            DB => conn.execute(
                sql,
                params![
                    r#"{"checked":true}"#,
                    200_i64,
                    ID,
                    rusqlite::types::Null,
                    rusqlite::types::Null
                ],
            ),
            MIGRATIONS => conn.execute(sql, params![vec![0_u8; 12], "2020-01-01T00:00:00Z", ID]),
            RECLAIM => conn.execute(sql, params![200_i64, ID, "local", 100_i64, properties]),
            RECEIPTS => conn.execute(
                sql,
                params![
                    300_i64,
                    200_i64,
                    ID,
                    "local",
                    100_i64,
                    "00000000-0000-4000-8000-000000000002"
                ],
            ),
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
            GTD_REPAIR => conn.execute(
                sql,
                params![
                    ID,
                    properties,
                    1_i64,
                    100_i64,
                    100_i64,
                    "integer",
                    "integer",
                    0_i64,
                    rusqlite::types::Null,
                    0_i64,
                    rusqlite::types::Null,
                    1_i64,
                    "done",
                    r#"{"originals":{}}"#,
                ],
            ),
            SCHEDULE => conn.execute(
                sql,
                params!["2026-09-09T00:00:00Z", 200_i64, ID, "local", properties],
            ),
            CURATION => conn.execute(sql, params![200_i64, "local", ID]),
            CREATE => conn.execute(sql, params!["census/key", ID, "local", "memory"]),
            OPERATIONS => conn.execute(
                &sql.replace("{key_clause}", ""),
                params!["active", 200_i64, ID, "local", "memory"],
            ),
            MESSAGE => conn.execute(sql, params!["census/key", ID, "local"]),
            COMM_INGEST => conn.execute(
                sql,
                params![
                    ID,
                    "local",
                    "census-ref",
                    "email",
                    "census@example.com",
                    300_i64,
                    200_i64
                ],
            ),
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
fn note_version_scanner_skips_tokio_tests_but_keeps_async_writers() {
    let mut scanner = Scanner::default();
    scanner.visit_file(
        &syn::parse_file(
            r#"
        #[tokio::test]
        async fn fixture() { call("UPDATE notes SET content='fixture'"); }
        #[tokio::test(flavor = "current_thread")]
        async fn configured_fixture() { call("UPDATE notes SET content='fixture'"); }
        #[::tokio::test]
        async fn absolute_fixture() { call("UPDATE notes SET content='fixture'"); }
        async fn live() { call("UPDATE notes SET content='live'"); }
        #[other::test]
        async fn unknown_attribute() { call("UPDATE notes SET content='live'"); }
        #[tokio::instrument]
        async fn another_tokio_attribute() { call("UPDATE notes SET content='live'"); }
        #[cfg(any(test, feature = "fault-injection"))]
        async fn feature() { call("UPDATE notes SET content='live' WHERE 1=0"); }
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
        [
            "live",
            "unknown_attribute",
            "another_tokio_attribute",
            "feature"
        ],
        "skip only the known Tokio test harness; retain every production-capable async writer"
    );
}

#[test]
fn plain_sql_insert_retains_the_note_version_initializer_guard() {
    let ordinary = "INSERT INTO notes (id, properties) VALUES (?1, ?2)";
    assert!(!note_writer(ordinary));
    assert_note_version_implicit(ordinary, "ordinary.sql");
    let explicit = "INSERT INTO notes (id, version, properties) VALUES (?1, 1, ?2)";
    assert!(!note_writer(explicit));
    assert!(std::panic::catch_unwind(|| {
        assert_note_version_implicit(explicit, "explicit.sql");
    })
    .is_err());
}

fn assert_initializer_refusal(owner: &str, check: impl FnOnce() + std::panic::UnwindSafe) {
    let failure = std::panic::catch_unwind(check).expect_err("explicit version must be refused");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("initializer refusal has a string diagnostic");
    assert!(
        message.contains(owner) && message.contains("explicitly initializes note.version"),
        "wrong refusal: {message}"
    );
}

#[test]
fn note_version_initializer_checks_later_inserts_and_replace() {
    for sql in [
        "\u{feff}INSERT INTO notes (id, version) VALUES (1, 2);",
        "INSERT INTO audit (id, version) VALUES (1, 2); INSERT INTO notes (id, version) VALUES (1, \
            2);",
        "INSERT INTO notes (id) VALUES (1); INSERT INTO main.notes (id, version) SELECT 2, 3;",
        "REPLACE INTO notes (id, version) VALUES (1, 2);",
        "WITH input(id) AS (SELECT 1) INSERT INTO notes AS target (id, version) SELECT id, 2 FROM \
            input;",
        "CREATE TRIGGER new_note AFTER INSERT ON audit BEGIN INSERT INTO notes (id, version) \
            VALUES (NEW.id, 2); END;",
    ] {
        assert_initializer_refusal("statements.sql", || {
            assert_note_version_implicit(sql, "statements.sql");
        });
    }
    for prefix in [
        "INSERT",
        "INSERT OR ROLLBACK",
        "INSERT OR ABORT",
        "INSERT OR REPLACE",
        "INSERT OR FAIL",
        "INSERT OR IGNORE",
        "REPLACE",
    ] {
        for target in [
            "notes",
            "main.notes",
            r#""main"."notes""#,
            "`main`.`notes`",
            "[main].[notes]",
            "'main'.'notes'",
        ] {
            let explicit = format!("{prefix} INTO {target} (id, \"version\") VALUES (1, 2);");
            assert_initializer_refusal("forms.sql", || {
                assert_note_version_implicit(&explicit, "forms.sql");
            });
            let ordinary = format!("{prefix} INTO {target} (id, content) VALUES (1, 'version');");
            assert_note_version_implicit(&ordinary, "forms.sql");
        }
    }
}

#[test]
fn note_version_initializer_respects_quotes_comments_and_column_boundaries() {
    for sql in [
        r#"INSERT INTO notes ("values", "select", "version") VALUES (1, 2, 3);"#,
        "insert/* target */into MAIN /* schema */ . [NoTeS] (id, [VeRsIoN]) values (1, 2);",
        "REPLACE INTO `notes` (id, `version`) SELECT 1, 2;",
        "INSERT INTO 'notes' ('id', 'version') VALUES (1, 2);",
        r#"INSERT INTO notes ("ver""sion", version) VALUES (1, 2);"#,
        "INSERT INTO notes (`ver``sion`, version) VALUES (1, 2);",
        "INSERT INTO notes ([semi;colon], version) VALUES (1, 2);",
        "/* outer /* inner */ INSERT INTO notes (version) VALUES (1);",
        "INSERT INTO audit (content) VALUES ('escaped ''; REPLACE INTO notes(version) VALUES \
            (1);'); REPLACE INTO notes (version) VALUES (2);",
    ] {
        assert_initializer_refusal("quoted.sql", || {
            assert_note_version_implicit(sql, "quoted.sql");
        });
    }
    for sql in [
        "INSERT INTO audit (version) VALUES (1); INSERT INTO notes (id, content) VALUES (2, \
            'version');",
        "INSERT INTO notes_seq (version) VALUES (1);",
        "INSERT INTO other.notes (version) VALUES (1);",
        r#"INSERT INTO "main.notes" (version) VALUES (1);"#,
        "INSERT INTO notes (id, versioned, version$tag, versioné) SELECT id, version, 1, 2 FROM \
            audit;",
        "INSERT INTO notes DEFAULT VALUES;",
        "\u{feff}INSERT INTO notes (id) VALUES (1);",
        "INSERT INTO notes (ver\u{feff}sion) VALUES (1);",
        "REPLACE INTO main.notes (id) VALUES (1);",
        "SELECT 'INSERT INTO notes(version) VALUES (1);';",
        r#"SELECT "INSERT", "INTO", "notes", "version";"#,
        r#"SELECT "escaped ""; INSERT INTO notes(version) VALUES (1);";"#,
        "SELECT 'escaped ''; INSERT INTO notes(version) VALUES (1);';",
        "SELECT `escaped ``; INSERT INTO notes(version) VALUES (1);`;",
        "SELECT [semicolon; INSERT INTO notes(version) VALUES (1)];",
        "-- INSERT INTO notes(version) VALUES (1);\nINSERT INTO notes(id) VALUES (1);",
        "/* REPLACE INTO notes(version) VALUES (1); */ INSERT INTO notes(id) VALUES (1);",
        "INSERT INTO notes (id, content) VALUES (1, 'version; INSERT INTO notes(version) VALUES \
            (2);');",
        // Comments separate tokens; punctuation prevents borrowing another statement's INTO.
        "INS/**/ERT INTO notes(version) VALUES (1);",
        "SELECT INSERT; SELECT INTO notes(version);",
        r#"SELECT "INSERT" INTO notes(version);"#,
    ] {
        assert_note_version_implicit(sql, "harmless.sql");
    }
}

#[test]
fn sql_file_inventory_applies_initializer_guard_before_writer_filtering() {
    let fixture = tempfile::tempdir().unwrap();
    let sql_dir = fixture.path().join("fixture").join("sql");
    std::fs::create_dir_all(sql_dir.join("nested")).unwrap();
    let writer = sql_dir.join("nested").join("writer.sql");
    std::fs::write(
        &writer,
        "UPDATE notes SET content = 'safe' WHERE id = 'fixture';",
    )
    .unwrap();
    std::fs::write(
        sql_dir.join("ignored.txt"),
        "INSERT INTO notes (version) VALUES (1);",
    )
    .unwrap();
    let insert = sql_dir.join("insert.sql");
    std::fs::write(
        &insert,
        "INSERT INTO audit (version) VALUES (1); INSERT INTO notes (id) VALUES (2);",
    )
    .unwrap();
    let expected = BTreeSet::from([writer
        .strip_prefix(fixture.path())
        .unwrap()
        .to_string_lossy()
        .into_owned()]);
    assert_eq!(sql_file_writer_inventory(fixture.path()), expected);

    for poisoned in [
        "\u{feff}INSERT INTO notes (version) VALUES (1);",
        "INSERT INTO audit (id) VALUES (1); INSERT INTO notes (id, version) VALUES (2, 3);",
        "INSERT INTO notes (id) VALUES (1); REPLACE INTO main.notes (id, version) VALUES (2, 3);",
    ] {
        assert!(
            !note_writer(poisoned),
            "the guard must precede the writer filter"
        );
        std::fs::write(&insert, poisoned).unwrap();
        assert_initializer_refusal(&insert.display().to_string(), || {
            sql_file_writer_inventory(fixture.path());
        });
    }
    std::fs::write(&insert, "REPLACE INTO notes (id) VALUES (1);").unwrap();
    assert_eq!(sql_file_writer_inventory(fixture.path()), expected);
}

fn sql_file_writer_inventory(root: &Path) -> BTreeSet<String> {
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let sql = entry.unwrap().path().join("sql");
        if sql.is_dir() {
            files(&sql, "sql", &mut sources);
        }
    }
    sources
        .into_iter()
        .filter(|path| {
            let sql = std::fs::read_to_string(path).unwrap();
            assert_note_version_implicit(&sql, &path.display().to_string());
            note_writer(&sql)
        })
        .map(|path| {
            path.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn note_version_sql_files_are_inventoried_and_trigger_is_the_only_exception() {
    let found = sql_file_writer_inventory(&root());
    assert_eq!(
        found,
        LEGACY_SQL_WRITERS
            .iter()
            .copied()
            .chain(APPLICATION_SQL_WRITERS.iter().map(|(asset, _, _)| *asset))
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

#[test]
fn static_sql_loaders_keep_named_writers_and_legacy_ownership() {
    let update = "UPDATE notes SET properties=?1 WHERE id=?2";
    let assets = StaticSqlSources::from([
        ("sample/sql/update.sql".into(), format!("{update}\n")),
        (
            LEGACY_SQL_WRITERS[0].into(),
            "UPDATE notes SET properties='{}'; CREATE INDEX fixture ON notes(id);".into(),
        ),
    ]);
    let source = "fn writer() { call(khive_runtime::sql!(\"update\")); }";
    let (loaded, links) =
        scan_sources_with_sql(&[("sample/src/lib.rs".into(), source.into())], &assets);
    let (inline, _) = scan_sources_with_sql(
        &[(
            "sample/src/lib.rs".into(),
            format!("fn writer() {{ call({update:?}); }}"),
        )],
        &StaticSqlSources::new(),
    );
    assert_eq!(loaded, inline);
    assert_eq!(
        links,
        BTreeSet::from([(
            "sample/sql/update.sql".into(),
            "sample/src/lib.rs".into(),
            "writer".into()
        )])
    );
    let (included, _) = scan_sources_with_sql(
        &[(
            "sample/src/lib.rs".into(),
            "fn writer() { call(include_str!(\"../sql/update.sql\")); }".into(),
        )],
        &assets,
    );
    assert_eq!(
        included[&("sample/src/lib.rs".into(), "writer".into())],
        format!("{update}\n")
    );
    let (legacy, legacy_links) = scan_sources_with_sql(
        &[(
            "khive-db/src/migrations.rs".into(),
            "const V5: &str = include_str!(\"../sql/005-unique-comm-external-id.sql\");".into(),
        )],
        &assets,
    );
    assert!(legacy.is_empty());
    assert!(legacy_links.is_empty());
}

#[test]
fn loaded_sql_cannot_hide_version_changes_duplicates_or_shadowing() {
    let source = "fn writer() { call(khive_runtime::sql!(\"update\")); }";
    for sql in [
        "INSERT INTO notes (id, version) VALUES (?1, 8)",
        "UPDATE {table} SET version=8",
    ] {
        let assets = StaticSqlSources::from([("sample/sql/update.sql".into(), sql.into())]);
        assert!(std::panic::catch_unwind(|| scan_sources_with_sql(
            &[("sample/src/lib.rs".into(), source.into())],
            &assets
        ))
        .is_err());
    }
    let assets = StaticSqlSources::from([(
        "sample/sql/update.sql".into(),
        "UPDATE notes SET version=8".into(),
    )]);
    let (found, _) = scan_sources_with_sql(&[("sample/src/lib.rs".into(), source.into())], &assets);
    assert!(!assignments_rule_out_version(
        &found[&("sample/src/lib.rs".into(), "writer".into())]
    ));
    for source in [
        "fn writer() { call(khive_runtime::sql!(\"update\")); call(khive_runtime::sql!(\"update\")); }",
        "fn writer() { use other as khive_runtime; call(khive_runtime::sql!(\"update\")); }",
        "mod khive_runtime {} fn writer() { call(khive_runtime::sql!(\"update\")); }",
    ] {
        assert!(std::panic::catch_unwind(|| scan_sources_with_sql(&[("sample/src/lib.rs".into(), source.into())], &assets)).is_err(), "{source}");
    }
    let positive = "mod unrelated { use unknown::*; } #[cfg(test)] mod hidden { use other as khive_runtime; } fn writer() { call(khive_runtime::sql!(\"update\")); }";
    assert_eq!(
        scan_sources_with_sql(&[("sample/src/lib.rs".into(), positive.into())], &assets)
            .0
            .len(),
        1
    );
}

#[test]
fn application_sql_inventory_rejects_orphans_and_wrong_callers() {
    let mut assets = StaticSqlSources::new();
    for asset in LEGACY_SQL_WRITERS
        .iter()
        .copied()
        .chain(APPLICATION_SQL_WRITERS.iter().map(|(asset, _, _)| *asset))
    {
        assets.insert(asset.into(), "UPDATE notes SET content='fixture'".into());
    }
    let links = APPLICATION_SQL_WRITERS
        .iter()
        .map(|(asset, path, owner)| (asset.to_string(), path.to_string(), owner.to_string()))
        .collect::<BTreeSet<_>>();
    assert_sql_ownership(&assets, &links);
    assert!(std::panic::catch_unwind(|| assert_sql_ownership(&assets, &BTreeSet::new())).is_err());
    let mut wrong = links.clone();
    let first = wrong.pop_first().unwrap();
    wrong.insert((first.0, first.1, "unrelated_writer".into()));
    assert!(std::panic::catch_unwind(|| assert_sql_ownership(&assets, &wrong)).is_err());
    assets.insert(
        "sample/sql/unreferenced.sql".into(),
        "UPDATE notes SET content='rogue'".into(),
    );
    assert!(std::panic::catch_unwind(|| assert_sql_ownership(&assets, &links)).is_err());
}

#[test]
fn markerless_route_census_rejects_empty_and_unlisted_production_callers() {
    let path = "khive-runtime/src/fixture.rs";
    let source = r#"
        async fn approved(store: Store, note: Note) { store.upsert_note(note).await; }
        #[cfg(test)] mod tests { fn ignored() { note_insert_keyed_statement(&note); } }
    "#;
    let scan = |source: &str| {
        scan_sources_with_routes(&[(path.into(), source.into())], &StaticSqlSources::new()).2
    };
    let inventory = [(
        path,
        "approved",
        "upsert_note",
        MarkerDisposition::StorageConstructor,
    )];
    assert_markerless_callers(&scan(source), &inventory);
    assert!(std::panic::catch_unwind(|| assert_markerless_callers(&scan(""), &inventory)).is_err());
    let added = format!("{source} fn unreviewed() {{ note_insert_keyed_statement(&note); }}");
    assert!(
        std::panic::catch_unwind(|| assert_markerless_callers(&scan(&added), &inventory)).is_err()
    );
    let transport = r#"async fn received(pool: Pool, request: RecipientCommit) {
        let recipient = RecipientTransportStore::new(pool);
        recipient.commit(request).await;
    }"#;
    assert_markerless_callers(
        &scan(transport),
        &[(
            path,
            "received",
            "RecipientTransportStore::commit",
            MarkerDisposition::FixedNonMemoryKind,
        )],
    );
    assert!(
        std::panic::catch_unwind(|| assert_markerless_callers(&scan(transport), &inventory))
            .is_err()
    );
}
