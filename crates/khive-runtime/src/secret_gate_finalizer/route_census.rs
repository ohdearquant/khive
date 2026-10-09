//! Source-side guard for ADR-115 Amendment 5's properties write population.
//! The scanner is pure over `(path, text)` pairs so mutations can exercise it
//! without building or loading another crate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;
use syn::{
    Attribute, Block, Expr, ExprCall, ExprLit, ExprMethodCall, FnArg, ImplItemFn, ItemConst,
    ItemEnum, ItemFn, ItemImpl, ItemMod, ItemStatic, ItemStruct, ItemTrait, ItemTraitAlias,
    ItemType, ItemUnion, ItemUse, Lit, Local, Macro, Pat, PatIdent, Stmt, UseTree,
};

use super::declaration::{
    Acceptance, Reservation, RouteInventoryEntry, RuntimeTableWriteInventoryEntry, Substrate,
    TransactionOwner, WriteClass, PINNED_MISSING_ACCEPTANCE, ROUTE_INVENTORY,
    RUNTIME_TABLE_WRITE_INVENTORY,
};

#[path = "route_census_parsed_sources.rs"]
mod parsed_sources;
#[path = "../../tests/support/static_sql_source.rs"]
mod static_sql_source;
use parsed_sources::{index_module_bindings, parse_production_sources, scan_source};
use static_sql_source::{CanonicalBindings, StaticSqlSources};

#[path = "route_census_store_methods.rs"]
mod store_methods;
use store_methods::STORE_WRITES;

// The complement is explicit, including readers: a new trait method of any
// spelling makes the census red until its properties behavior is classified.
const NON_PROPERTIES_STORE_METHODS: &[&str] = &[
    "get_entity",
    "delete_entity",
    "query_entities",
    "query_entities_count_free",
    "entity_sequence",
    "query_entities_after",
    "count_entities",
    "count_entities_by_type",
    "get_entity_including_deleted",
    "get_live_notes_by_key",
    "query_keyed_notes",
    "get_note",
    "get_note_including_deleted",
    "delete_note",
    "query_notes",
    "query_notes_count_free",
    "query_notes_filtered",
    "query_notes_filtered_count_free",
    "count_notes_filtered_in_snapshot",
    "count_notes_filtered_bounded_in_snapshot",
    "note_sequence",
    "query_notes_filtered_after",
    "query_notes_filtered_bounded",
    "count_notes",
    "count_notes_in_namespaces",
    "get_notes_batch",
    "get_notes_batch_including_deleted",
    "get_note_visibility_batch",
];

const ENTITY_BUILDERS: &[&str] = &[
    "entity_upsert_statement",
    "entity_insert_if_absent_statement",
    "entity_replace_if_unchanged_statement",
];
const NOTE_BUILDERS: &[&str] = &[
    "note_insert_if_absent_statement",
    "note_insert_keyed_statement",
    "note_upsert_statement",
    "note_replace_if_unchanged_statement",
    "note_update_properties_statement",
    "note_set_property_statement",
];

// External references to these khive-db statements bypass the builder and
// store-method detectors. Both statements insert a whole note properties
// object, even when an ON CONFLICT clause does not replace an existing row.
const NOTE_PROPERTY_SQL_CONSTANTS: &[&str] = &["NOTE_UPSERT_SQL", "NOTE_INSERT_IF_ABSENT_SQL"];

// Evidence for a site that reaches a note SQL constant only if a name that the
// census cannot classify is not a type. The census refuses it wherever it
// occurs, instead of routing or dropping it.
const UNRESOLVED_CONSTANT: &str = "UNRESOLVED note SQL constant";

// Evidence prefix for a path that resolves to a note SQL constant.
const SQL_CONSTANT_EVIDENCE: &str = "SQL constant ";

// Evidence prefix for a statement that writes a guarded table in a spelling the
// census cannot classify. The census refuses it wherever it occurs, because a
// spelling it cannot read is a route it cannot inventory.
const UNCLASSIFIED_SQL: &str = "UNCLASSIFIED guarded-table SQL write";

#[derive(Debug, Clone, PartialEq, Eq)]
enum DetectedClass {
    WholeObject,
    SingleKey(String),
    FixedKeySet(BTreeSet<String>),
}

fn canonical_key_path(path: &str) -> Option<String> {
    // Canonicalization sorts and deduplicates the set; labels remain byte-exact.
    bare_top_level_path(path).then(|| path.to_owned())
}

fn detected_keys(class: &DetectedClass) -> Option<BTreeSet<String>> {
    match class {
        DetectedClass::WholeObject => None,
        DetectedClass::SingleKey(path) => Some(BTreeSet::from([canonical_key_path(path)?])),
        DetectedClass::FixedKeySet(paths) => Some(paths.clone()),
    }
}

fn detected_key_class(paths: BTreeSet<String>) -> DetectedClass {
    if paths.len() == 1 {
        DetectedClass::SingleKey(paths.into_iter().next().expect("nonempty key set"))
    } else {
        DetectedClass::FixedKeySet(paths)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteClass {
    Application,
    Migration,
}

#[derive(Debug, Clone)]
struct Site {
    key: String,
    target: Substrate,
    route_class: RouteClass,
    class: DetectedClass,
    write_count: usize,
    calls: BTreeSet<String>,
    evidence: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct RuntimeTableSite {
    key: String,
    write_count: usize,
    calls: BTreeSet<String>,
    evidence: BTreeSet<String>,
}

#[derive(Debug)]
struct SourcePopulation {
    properties: Vec<Site>,
    runtime_tables: Vec<RuntimeTableSite>,
    /// SQL assets that a followed loader resolved, for the asset-side reach check.
    resolved_sql: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlConstantReference {
    site: String,
    constant: String,
}

struct ScannedSource {
    sql_errors: Vec<String>,
    resolved_sql: BTreeSet<String>,
    sites: Vec<Site>,
    runtime_tables: Vec<RuntimeTableSite>,
    /// Every expression path has an ordinal, whether or not it resolves. Both
    /// passes visit the same AST, so a sibling reference cannot stand in for it.
    constant_references: BTreeMap<usize, SqlConstantReference>,
}

fn attribute_name(attr: &Attribute) -> String {
    attr.path()
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn test_only(attrs: &[Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| items.iter().any(requires_test)),
            syn::Meta::List(list) if list.path.is_ident("any") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| !items.is_empty() && items.iter().all(requires_test)),
            _ => false,
        }
    }

    attrs.iter().any(|attr| {
        let name = attribute_name(attr);
        if name == "test" || name == "tokio::test" {
            return true;
        }
        name == "cfg"
            && matches!(&attr.meta, syn::Meta::List(list)
                if list.parse_args::<syn::Meta>().is_ok_and(|meta| requires_test(&meta)))
    })
}

/// One bare top-level identifier, optionally following `$` and `.`.
fn bare_top_level_path(path: &str) -> bool {
    let segment = path.strip_prefix("$.").unwrap_or(path);
    if segment == "$" || segment.is_empty() || segment.contains('.') {
        return false;
    }
    let mut bytes = segment.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Some(value.value()),
        Expr::Reference(reference) => literal_string(&reference.expr),
        Expr::Paren(paren) => literal_string(&paren.expr),
        _ => None,
    }
}

/// The guarded table that one word names, if any.
fn guarded_table(word: &str) -> Option<Substrate> {
    match word {
        "ENTITIES" => Some(Substrate::Entity),
        "NOTES" => Some(Substrate::Note),
        _ => None,
    }
}

/// Find the guarded table at the front of a write target. The flag says whether
/// it was reached bare or through the `main` or `temp` schema, and the count is
/// how many words the table and its schema took.
fn guarded_head(head: &[&str]) -> Option<(Substrate, bool, usize)> {
    if let Some(target) = head.first().copied().and_then(guarded_table) {
        return Some((target, true, 1));
    }
    let target = head.get(1).copied().and_then(guarded_table)?;
    Some((target, matches!(head[0], "MAIN" | "TEMP"), 2))
}

/// What may stand between an UPDATE's table and its SET: an optional alias and
/// an optional index hint.
fn update_tail_is_known(tail: &[&str]) -> bool {
    let rest = match tail {
        ["AS", _, rest @ ..] => rest,
        [alias, rest @ ..] if !matches!(*alias, "INDEXED" | "NOT") => rest,
        rest => rest,
    };
    matches!(rest, [] | ["NOT", "INDEXED"] | ["INDEXED", "BY", _])
}

/// A literal names one substrate, and entities win a tie as they always have.
fn entity_first(targets: &[Substrate]) -> Option<Substrate> {
    targets
        .iter()
        .copied()
        .find(|target| *target == Substrate::Entity)
        .or_else(|| targets.first().copied())
}

/// Read every insert, replace and update statement in a literal that targets a
/// guarded table. The first collection contains one substrate per classified
/// write occurrence. The second contains one substrate per occurrence that
/// names a guarded table in a spelling the census cannot read: the census must
/// refuse it, because a write it cannot classify is a route it cannot inventory.
fn sql_write_occurrences(literal: &str) -> (Vec<Substrate>, Vec<Substrate>) {
    // Match SQL tokens rather than keeping SQL-shaped matcher literals in this
    // census: other source censuses must not mistake those for writer sites.
    // Comments are not SQL, so they are dropped before the words are read, and
    // each parenthesis becomes a word of its own so that a column list is never
    // read as part of the write target.
    let spaced = sql_statements(literal)
        .join(" ")
        .to_ascii_uppercase()
        .replace('(', " ( ");
    let words = spaced
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '('
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let mut routes = Vec::new();
    let mut unclassified = Vec::new();
    for (index, word) in words.iter().enumerate() {
        // INSERT OR REPLACE is one write, not an INSERT plus a REPLACE.
        if *word == "REPLACE" && index > 0 && words[index - 1] == "OR" {
            continue;
        }
        let mut at = index + 1;
        match *word {
            // REPLACE only starts a statement when INTO follows it.
            "INSERT" | "REPLACE" => {
                if *word == "INSERT" && words.get(at) == Some(&"OR") {
                    at += 2;
                }
                if words.get(at) != Some(&"INTO") {
                    continue;
                }
                let head = words[at + 1..]
                    .iter()
                    .copied()
                    .take_while(|candidate| {
                        !matches!(*candidate, "(" | "VALUES" | "SELECT" | "DEFAULT" | "WITH")
                    })
                    .take(4)
                    .collect::<Vec<_>>();
                match guarded_head(&head) {
                    Some((target, true, _)) => routes.push(target),
                    _ => unclassified.extend(head.iter().take(2).copied().find_map(guarded_table)),
                }
            }
            "UPDATE" => {
                if words.get(at) == Some(&"OR") {
                    at += 2;
                }
                let rest = words.get(at..).unwrap_or_default();
                let Some(set_at) = rest
                    .iter()
                    .position(|candidate| matches!(*candidate, "SET" | "("))
                else {
                    continue;
                };
                if rest[set_at] != "SET" {
                    continue;
                }
                let head = &rest[..set_at];
                let after_set = &rest[set_at + 1..];
                // A trigger header names its table after ON, never directly
                // after UPDATE, and no UPDATE target is longer than a schema,
                // a table, an alias and an index hint.
                if matches!(head.first().copied(), Some("ON" | "OF")) || head.len() > 7 {
                    continue;
                }
                // An update counts when PROPERTIES appears anywhere after SET.
                // Cutting the SET clause at the first WHERE would miss a write
                // that follows a subquery or a string containing that word, so
                // an update that only reads properties is reported as well.
                if !after_set.contains(&"PROPERTIES") {
                    continue;
                }
                match guarded_head(head) {
                    Some((target, true, used)) if update_tail_is_known(&head[used..]) => {
                        routes.push(target);
                    }
                    Some((target, _, _)) => unclassified.push(target),
                    None => {}
                }
            }
            _ => {}
        }
    }
    (routes, unclassified)
}

fn sql_write_shapes(literal: &str) -> (Option<Substrate>, Option<Substrate>) {
    let (routes, unclassified) = sql_write_occurrences(literal);
    (entity_first(&routes), entity_first(&unclassified))
}

fn sql_target(literal: &str) -> Option<Substrate> {
    sql_write_shapes(literal).0
}

fn sql_unclassified_write(literal: &str) -> Option<Substrate> {
    sql_write_shapes(literal).1
}

/// Count table-position placeholders without resolving their runtime values.
fn runtime_table_write_count(literal: &str) -> usize {
    let mut count = 0;
    for statement in sql_statements(literal) {
        // The flag distinguishes SQL words from quoted text and placeholders,
        // so an UPDATE inside a value string is not a statement keyword.
        let mut tokens: Vec<(String, bool)> = Vec::new();
        let mut chars = statement.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch.is_whitespace() {
                continue;
            }
            if ch.is_ascii_alphanumeric() || ch == '_' {
                let mut word = String::from(ch);
                while chars
                    .peek()
                    .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
                {
                    word.push(chars.next().expect("peeked word"));
                }
                tokens.push((word.to_ascii_uppercase(), true));
            } else if matches!(ch, '\'' | '"' | '`' | '[' | '{') {
                let closing = match ch {
                    '[' => ']',
                    '{' => '}',
                    _ => ch,
                };
                let mut value = String::from(ch);
                while let Some(next) = chars.next() {
                    value.push(next);
                    if next == closing {
                        if ch != '{' && chars.peek() == Some(&closing) {
                            value.push(chars.next().expect("peeked escaped quote"));
                        } else {
                            break;
                        }
                    }
                }
                tokens.push((value, false));
            } else {
                tokens.push((ch.to_string(), false));
            }
        }
        let word_at = |at: usize, word: &str| {
            tokens
                .get(at)
                .is_some_and(|(actual, is_word)| *is_word && actual == word)
        };
        let placeholder_at = |at: usize| {
            tokens
                .get(at)
                .is_some_and(|(token, _)| token.contains('{') && token.contains('}'))
                || (tokens.get(at + 1).is_some_and(|(token, _)| token == ".")
                    && tokens
                        .get(at + 2)
                        .is_some_and(|(token, _)| token.contains('{') && token.contains('}')))
        };
        for at in 0..tokens.len() {
            let insert = word_at(at, "INSERT");
            let replace = word_at(at, "REPLACE") && (at == 0 || !word_at(at - 1, "OR"));
            let is_update = word_at(at, "UPDATE");
            if !insert && !replace && !is_update {
                continue;
            }
            let mut target = at + 1;
            if (insert || is_update) && word_at(target, "OR") {
                target += 2;
            }
            if insert || replace {
                if !word_at(target, "INTO") {
                    continue;
                }
                target += 1;
            } else if !(target..tokens.len()).any(|index| word_at(index, "SET")) {
                // A display string such as "timing fixture update {id}" is
                // not an UPDATE statement without its SET clause.
                continue;
            }
            if placeholder_at(target) {
                count += 1;
            }
        }
    }
    count
}

#[derive(Debug, PartialEq, Eq)]
enum SqlToken {
    Word(String),
    String(String),
    Symbol(char),
}

/// Tokenize the narrow SQL form the census can prove. Quoted identifiers and
/// comments remain opaque rather than being mistaken for assignment syntax.
fn sql_tokens(sql: &str) -> Option<Vec<SqlToken>> {
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '\'' {
            let mut value = String::new();
            loop {
                let next = chars.next()?;
                if next == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
                value.push(next);
            }
            tokens.push(SqlToken::String(value));
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let mut word = String::from(ch);
            while chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
            {
                word.push(chars.next()?);
            }
            tokens.push(SqlToken::Word(word.to_ascii_lowercase()));
        } else if matches!(ch, '"' | '`' | '[' | ']')
            || (ch == '-' && chars.peek() == Some(&'-'))
            || (ch == '/' && chars.peek() == Some(&'*'))
        {
            return None;
        } else {
            tokens.push(SqlToken::Symbol(ch));
        }
    }
    Some(tokens)
}

fn sql_word(token: &SqlToken, word: &str) -> bool {
    matches!(token, SqlToken::Word(actual) if actual == word)
}

/// Split arguments or assignments only at their own parenthesis depth.
fn sql_parts(tokens: &[SqlToken]) -> Option<Vec<&[SqlToken]>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            SqlToken::Symbol('(') => depth += 1,
            SqlToken::Symbol(')') => depth = depth.checked_sub(1)?,
            SqlToken::Symbol(',') if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&tokens[start..]);
    parts.iter().all(|part| !part.is_empty()).then_some(parts)
}

/// Only the first argument's transformations can widen the properties write.
/// Path arguments have fixed positions: odd positions after json_set's input,
/// and every argument after json_remove's input. Values and WHERE literals
/// never contribute a key.
fn sql_json_key_paths(tokens: &[SqlToken], paths: &mut BTreeSet<String>) -> Option<()> {
    if tokens.len() == 1 && sql_word(&tokens[0], "properties") {
        return Some(());
    }
    let [SqlToken::Word(function), SqlToken::Symbol('('), args @ .., SqlToken::Symbol(')')] =
        tokens
    else {
        return None;
    };
    if !matches!(function.as_str(), "json_set" | "json_remove") {
        return None;
    }
    let args = sql_parts(args)?;
    let (input, changes) = args.split_first()?;
    sql_json_key_paths(input, paths)?;
    let path_args = if function == "json_set" {
        if changes.is_empty() || changes.len() % 2 != 0 {
            return None;
        }
        changes.iter().step_by(2).copied().collect::<Vec<_>>()
    } else {
        if changes.is_empty() {
            return None;
        }
        changes.to_vec()
    };
    for arg in path_args {
        let [SqlToken::String(path)] = arg else {
            return None;
        };
        if !path.starts_with("$.") || !bare_top_level_path(path) {
            return None;
        }
        paths.insert(path.clone());
    }
    Some(())
}

/// Prove the complete key set of one properties assignment in a plain UPDATE.
/// Unknown inputs, dynamic paths and opaque transformations stay whole-object.
fn sql_fixed_key_paths(literal: &str) -> Option<BTreeSet<String>> {
    let tokens = sql_tokens(literal)?;
    if tokens.len() < 4
        || tokens.contains(&SqlToken::Symbol(';'))
        || !sql_word(&tokens[0], "update")
        || !(sql_word(&tokens[1], "notes") || sql_word(&tokens[1], "entities"))
        || !sql_word(&tokens[2], "set")
    {
        return None;
    }
    let mut depth = 0usize;
    let mut end = tokens.len();
    for (index, token) in tokens.iter().enumerate().skip(3) {
        match token {
            SqlToken::Symbol('(') => depth += 1,
            SqlToken::Symbol(')') => depth = depth.checked_sub(1)?,
            SqlToken::Word(word) if word == "where" && depth == 0 => {
                end = index;
                break;
            }
            SqlToken::Symbol(';') => return None,
            _ => {}
        }
    }
    let mut paths = None;
    for assignment in sql_parts(&tokens[3..end])? {
        let [SqlToken::Word(column), SqlToken::Symbol('='), expression @ ..] = assignment else {
            return None;
        };
        if column == "properties" {
            if paths.is_some() {
                return None;
            }
            let mut keys = BTreeSet::new();
            sql_json_key_paths(expression, &mut keys)?;
            if keys.is_empty() {
                return None;
            }
            paths = Some(keys);
        }
    }
    paths
}

fn sql_single_key_path(literal: &str) -> Option<String> {
    let paths = sql_fixed_key_paths(literal)?;
    (paths.len() == 1)
        .then(|| paths.into_iter().next())
        .flatten()
}

fn macro_strings(tokens: TokenStream, strings: &mut Vec<String>) {
    for token in tokens {
        match token {
            TokenTree::Literal(literal) => {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    strings.push(value.value());
                }
            }
            TokenTree::Group(group) => macro_strings(group.stream(), strings),
            _ => {}
        }
    }
}

/// What a name in scope denotes, as far as the census needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    /// A pattern binding (argument, `let`, closure input). It shadows a value
    /// name but never the leading segment of a path, which Rust resolves in
    /// the module namespace.
    Local,
    /// An import whose item the census cannot see, such as one from a crate
    /// outside the workspace. Its name cannot tell a type from a function or
    /// constant. A type lives in the module namespace, so Rust resolves the
    /// head of a path through it and never reaches a module or crate of the
    /// same name, while a function or constant leaves that module or crate
    /// visible. The strict census resolution therefore stops at it, and the
    /// lenient one looks past it.
    Other,
    /// A struct, enum, union, trait or type alias declared in a scanned
    /// module. It lives in the module namespace, so it hides a child module
    /// or crate of the same name at the head of a path: `Ty::NAME` is the
    /// type's associated item, never an item of a module or crate `Ty`.
    Type,
    /// A function, constant or static declared in a scanned module. It lives
    /// in the value namespace, so it never hides a module or crate.
    Value,
    /// A note properties SQL constant, by its declared name.
    Constant(String),
    /// A scanned module, so a path through this name resolves inside it.
    Module(ModuleId),
}

type SqlBindings = BTreeMap<String, Binding>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModuleId {
    root: String,
    segments: Vec<String>,
}

type ModuleBindings = BTreeMap<ModuleId, SqlBindings>;

/// Name that a `use` tree records for a glob import. It is not an identifier,
/// so it cannot collide with an imported name. A scope binds it to `Other`
/// while it holds a glob import that the census cannot open.
const GLOB_IMPORT: &str = "*";

fn use_tree_imports(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    imports: &mut Vec<(String, Vec<String>)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            use_tree_imports(&path.tree, prefix, imports);
            prefix.pop();
        }
        UseTree::Group(group) => {
            for item in &group.items {
                use_tree_imports(item, prefix, imports);
            }
        }
        // `{self}` and `{self as alias}` import the module the prefix names.
        UseTree::Name(name) if name.ident == "self" => {
            if let Some(module) = prefix.last() {
                imports.push((module.clone(), prefix.clone()));
            }
        }
        UseTree::Rename(rename) if rename.ident == "self" => {
            imports.push((rename.rename.to_string(), prefix.clone()));
        }
        UseTree::Name(name) => {
            let name = name.ident.to_string();
            let mut path = prefix.clone();
            path.push(name.clone());
            imports.push((name, path));
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            path.push(rename.ident.to_string());
            imports.push((rename.rename.to_string(), path));
        }
        UseTree::Glob(_) => imports.push((GLOB_IMPORT.to_owned(), prefix.clone())),
    }
}

/// A binding the census follows: a note SQL constant or a scanned module.
fn resolved(binding: Option<&Binding>) -> Option<Binding> {
    match binding {
        Some(binding @ (Binding::Constant(_) | Binding::Module(_))) => Some(binding.clone()),
        _ => None,
    }
}

fn import_target(
    path: &[String],
    known: &SqlBindings,
    parents: &[SqlBindings],
    current_module: Option<&SqlBindings>,
    module_id: &ModuleId,
    modules: &ModuleBindings,
    strict: bool,
) -> Option<Binding> {
    let (original, prefix) = path.split_last()?;
    let lookup = |scope: &SqlBindings| resolved(scope.get(original));
    match prefix {
        [] => lookup(known).or_else(|| current_module.and_then(&lookup)),
        [qualifier] if qualifier.as_str() == "self" => {
            current_module.and_then(&lookup).or_else(|| lookup(known))
        }
        [qualifier] if qualifier.as_str() == "crate" => parents
            .first()
            .or(current_module)
            .and_then(&lookup)
            .or_else(|| lookup(known)),
        _ if prefix.iter().all(|qualifier| qualifier.as_str() == "super") => parents
            .len()
            .checked_sub(prefix.len())
            .and_then(|index| parents.get(index))
            .and_then(&lookup),
        _ => None,
    }
    .or_else(|| {
        qualified_target(
            path,
            module_id,
            modules,
            &|first| import_scope_binding(first, known, current_module),
            strict,
        )
    })
    .or_else(|| {
        declared_target(path, module_id, modules, &|first| {
            import_scope_binding(first, known, current_module)
        })
    })
}

/// The binding that a leading path segment names through the imports in
/// scope. The nearest scope that binds the name decides, as in Rust.
fn import_scope_binding(
    first: &str,
    known: &SqlBindings,
    current_module: Option<&SqlBindings>,
) -> Option<Binding> {
    std::iter::once(known)
        .chain(current_module)
        .find_map(|scope| scope.get(first))
        .cloned()
}

fn child_module(parent: &ModuleId, name: &str, modules: &ModuleBindings) -> Option<ModuleId> {
    let mut child = parent.clone();
    child.segments.push(name.to_owned());
    modules.contains_key(&child).then_some(child)
}

/// The library root of the workspace crate that an extern path names. Paths
/// spell the crate directory `khive-db` as `khive_db`.
fn crate_root(name: &str, modules: &ModuleBindings) -> Option<ModuleId> {
    modules
        .keys()
        .find(|module| {
            module.segments.is_empty()
                && module.root.strip_suffix("/src/lib.rs").is_some_and(|dir| {
                    dir.len() == name.len()
                        && dir
                            .bytes()
                            .zip(name.bytes())
                            .all(|(dir, name)| dir == name || (dir == b'-' && name == b'_'))
                })
        })
        .cloned()
}

/// The scanned module a path prefix names, or `None` when the prefix leaves
/// the scanned sources. A leading plain name resolves in Rust's order: an
/// import in scope, then a child module, then a workspace crate. A type
/// import hides a child module or crate of the same name, so it ends the
/// lookup. The census cannot tell a type from a function or constant by the
/// name of an import it cannot see, and a glob import it cannot open may bring
/// a type of any name. A `strict` resolution ends the lookup at either, and a
/// lenient one looks past them to the child module or crate. Each later
/// segment follows a child module or a re-exported module alias.
fn resolve_module(
    prefix: &[String],
    current: &ModuleId,
    modules: &ModuleBindings,
    in_scope: &dyn Fn(&str) -> Option<Binding>,
    strict: bool,
) -> Option<ModuleId> {
    let Some((first, rest)) = prefix.split_first() else {
        return Some(current.clone());
    };
    let (mut module, rest) = match first.as_str() {
        "crate" => (
            ModuleId {
                root: current.root.clone(),
                segments: Vec::new(),
            },
            rest,
        ),
        "self" => (current.clone(), rest),
        "super" => {
            let depth = prefix
                .iter()
                .take_while(|part| part.as_str() == "super")
                .count();
            let keep = current.segments.len().checked_sub(depth)?;
            let mut parent = current.clone();
            parent.segments.truncate(keep);
            (parent, &prefix[depth..])
        }
        name => {
            let bound = in_scope(name);
            (
                match bound {
                    Some(Binding::Module(module)) => module,
                    Some(Binding::Type) => return None,
                    _ if strict
                        && (bound == Some(Binding::Other) || in_scope(GLOB_IMPORT).is_some()) =>
                    {
                        return None;
                    }
                    _ => child_module(current, name, modules)
                        .or_else(|| crate_root(name, modules))?,
                },
                rest,
            )
        }
    };
    for part in rest {
        if matches!(part.as_str(), "crate" | "self" | "super") {
            return None;
        }
        module = match modules.get(&module).and_then(|bindings| bindings.get(part)) {
            Some(Binding::Module(target)) => target.clone(),
            _ => child_module(&module, part, modules)?,
        };
    }
    Some(module)
}

fn qualified_target(
    path: &[String],
    current: &ModuleId,
    modules: &ModuleBindings,
    in_scope: &dyn Fn(&str) -> Option<Binding>,
    strict: bool,
) -> Option<Binding> {
    let (name, prefix) = path.split_last()?;
    // Synthetic source populations may omit khive-db itself. Only its exact
    // known origin path is recognized in that case, after honoring a type or
    // opaque import at the head. A terminal name alone grants no identity.
    let origin = prefix == ["khive_db", "stores", "note"]
        && NOTE_PROPERTY_SQL_CONSTANTS.contains(&name.as_str());
    let module = resolve_module(prefix, current, modules, in_scope, strict);
    if module.is_none()
        && origin
        && crate_root("khive_db", modules).is_none()
        && child_module(current, "khive_db", modules).is_none()
    {
        let head = in_scope("khive_db");
        if head != Some(Binding::Type)
            && !matches!(&head, Some(Binding::Module(_)))
            && (!strict || (head != Some(Binding::Other) && in_scope(GLOB_IMPORT).is_none()))
        {
            return Some(Binding::Constant(name.clone()));
        }
    }
    let module = module?;
    if module.root == "khive-db/src/lib.rs"
        && module.segments == ["stores", "note"]
        && NOTE_PROPERTY_SQL_CONSTANTS.contains(&name.as_str())
    {
        return Some(Binding::Constant(name.clone()));
    }
    resolved(modules.get(&module).and_then(|bindings| bindings.get(name)))
        .or_else(|| child_module(&module, name, modules).map(Binding::Module))
        // `use khive_db;` names the workspace crate itself.
        .or_else(|| {
            prefix
                .is_empty()
                .then(|| crate_root(name, modules))
                .flatten()
                .map(Binding::Module)
        })
}

/// The namespace of the item a path names when a scanned module declares it,
/// as a type or as a value.
fn declared_target(
    path: &[String],
    current: &ModuleId,
    modules: &ModuleBindings,
    in_scope: &dyn Fn(&str) -> Option<Binding>,
) -> Option<Binding> {
    let (name, prefix) = path.split_last()?;
    // A head bound to an unclassified import still names the module whose
    // declarations classify this import; the classification never routes.
    let module = resolve_module(prefix, current, modules, in_scope, false)?;
    modules
        .get(&module)?
        .get(name)
        .filter(|binding| matches!(binding, Binding::Type | Binding::Value))
        .cloned()
}

/// The names a glob import of `module` brings: its resolved imports, glob
/// imports included, its types and its child modules. A declared value is not
/// listed: it never hides a module, so a path resolves the same without it.
fn glob_names(module: &ModuleId, modules: &ModuleBindings) -> Vec<(String, Binding)> {
    let mut names = modules
        .get(module)
        .into_iter()
        .flatten()
        .filter_map(|(name, binding)| {
            resolved(Some(binding))
                .or_else(|| (*binding == Binding::Type).then(|| binding.clone()))
                .map(|binding| (name.clone(), binding))
        })
        .collect::<Vec<_>>();
    names.extend(
        modules
            .keys()
            .filter(|child| {
                child.root == module.root
                    && child.segments.len() == module.segments.len() + 1
                    && child.segments.starts_with(&module.segments)
            })
            .filter_map(|child| {
                child
                    .segments
                    .last()
                    .map(|name| (name.clone(), Binding::Module(child.clone())))
            }),
    );
    names
}

fn resolve_imports(
    imports: &[(String, Vec<String>)],
    parents: &[SqlBindings],
    current_module: Option<&SqlBindings>,
    module_id: &ModuleId,
    modules: &ModuleBindings,
    strict: bool,
) -> SqlBindings {
    let mut bindings = imports
        .iter()
        .filter(|(name, _)| name != GLOB_IMPORT)
        .map(|(name, _)| (name.clone(), Binding::Other))
        .collect::<SqlBindings>();
    for _ in 0..=imports.len() {
        let known = bindings.clone();
        for (name, path) in imports {
            if name == GLOB_IMPORT {
                continue;
            }
            bindings.insert(
                name.clone(),
                import_target(
                    path,
                    &known,
                    parents,
                    current_module,
                    module_id,
                    modules,
                    strict,
                )
                .unwrap_or(Binding::Other),
            );
        }
        // Every explicit name is already bound, and explicit imports shadow
        // glob imports, so a glob only fills the names left unbound.
        let mut opaque_glob = false;
        for (_, path) in imports.iter().filter(|(name, _)| name == GLOB_IMPORT) {
            // The marker for an opaque glob must not hide the module that a
            // glob path names, or one opaque glob would make every glob opaque.
            let Some(source) = resolve_module(
                path,
                module_id,
                modules,
                &|first| {
                    (first != GLOB_IMPORT)
                        .then(|| import_scope_binding(first, &known, current_module))
                        .flatten()
                },
                strict,
            ) else {
                // A glob outside the scanned sources may bring a type of any
                // name, which then hides a same-named module or crate.
                opaque_glob = true;
                continue;
            };
            for (name, binding) in glob_names(&source, modules) {
                // A module's own child module is an item of that module, and
                // an item shadows a glob import of the same name in the same
                // namespace. A child module lives in the type namespace, so it
                // shadows a glob-imported module or type but never a
                // glob-imported constant, which a value position still reaches. A block's
                // glob import still shadows the enclosing module's items.
                if current_module.is_none()
                    && matches!(binding, Binding::Module(_) | Binding::Type)
                    && child_module(module_id, &name, modules).is_some()
                {
                    continue;
                }
                bindings.entry(name).or_insert(binding);
            }
        }
        if opaque_glob {
            bindings.insert(GLOB_IMPORT.to_owned(), Binding::Other);
        } else {
            bindings.remove(GLOB_IMPORT);
        }
        if known == bindings {
            break;
        }
    }
    bindings
}

fn use_bindings<'a>(
    uses: impl Iterator<Item = &'a ItemUse>,
    parents: &[SqlBindings],
    current_module: Option<&SqlBindings>,
    module_id: &ModuleId,
    modules: &ModuleBindings,
    strict: bool,
) -> SqlBindings {
    let mut imports = Vec::new();
    for item in uses {
        use_tree_imports(&item.tree, &mut Vec::new(), &mut imports);
    }
    resolve_imports(
        &imports,
        parents,
        current_module,
        module_id,
        modules,
        strict,
    )
}

#[derive(Default)]
struct BoundNames(BTreeSet<String>);

impl<'ast> Visit<'ast> for BoundNames {
    fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
        self.0.insert(pat.ident.to_string());
        syn::visit::visit_pat_ident(self, pat);
    }
}

fn shadow_bindings<'a>(patterns: impl Iterator<Item = &'a Pat>) -> SqlBindings {
    let mut names = BoundNames::default();
    for pattern in patterns {
        names.visit_pat(pattern);
    }
    names
        .0
        .into_iter()
        .map(|name| (name, Binding::Local))
        .collect()
}

struct SourceCollector<'modules> {
    sql_sources: &'modules StaticSqlSources,
    loader_bindings: CanonicalBindings,
    sql_errors: Vec<String>,
    resolved_sql: BTreeSet<String>,
    path: String,
    scope: Vec<String>,
    sites: BTreeMap<String, Site>,
    runtime_tables: BTreeMap<String, RuntimeTableSite>,
    all_calls: BTreeMap<String, BTreeSet<String>>,
    bindings: Vec<SqlBindings>,
    parent_module_bindings: Vec<SqlBindings>,
    module_id: ModuleId,
    modules: &'modules ModuleBindings,
    strict: bool,
    path_ordinal: usize,
    constant_references: BTreeMap<usize, SqlConstantReference>,
}

impl<'modules> SourceCollector<'modules> {
    fn key(&self) -> String {
        format!("{}::{}", self.path, self.scope.join("::"))
    }

    fn call(&mut self, function: &str) {
        let key = self.key();
        self.all_calls
            .entry(key.clone())
            .or_default()
            .insert(function.to_owned());
        if let Some(site) = self.sites.get_mut(&key) {
            site.calls.insert(function.to_owned());
        }
        if let Some(site) = self.runtime_tables.get_mut(&key) {
            site.calls.insert(function.to_owned());
        }
    }

    fn record(
        &mut self,
        target: Substrate,
        class: DetectedClass,
        evidence: String,
        write_count: usize,
    ) {
        let key = self.key();
        let calls = self.all_calls.get(&key).cloned().unwrap_or_default();
        let site = self.sites.entry(key.clone()).or_insert_with(|| Site {
            key: key.clone(),
            target,
            route_class: RouteClass::Application,
            class: class.clone(),
            write_count: 0,
            calls,
            evidence: BTreeSet::new(),
        });
        site.write_count += write_count;
        // A single function that writes both substrates cannot be described
        // by the current target enum. Keep both observations visible rather
        // than silently selecting the first one.
        if site.target != target {
            site.evidence.insert("MIXED_ENTITY_AND_NOTE_TARGETS".into());
        }
        if site.class != class {
            site.class = match (detected_keys(&site.class), detected_keys(&class)) {
                (Some(mut existing), Some(additional)) => {
                    existing.extend(additional);
                    detected_key_class(existing)
                }
                _ => DetectedClass::WholeObject,
            };
        }
        site.evidence.insert(evidence);
    }

    /// The binding that a leading path segment names through the imports in
    /// scope. Pattern bindings live in the value namespace and never shadow it.
    fn scoped_binding(&self, first: &str) -> Option<Binding> {
        self.bindings
            .iter()
            .rev()
            .filter_map(|scope| scope.get(first))
            .find(|binding| **binding != Binding::Local)
            .cloned()
    }

    fn record_sql(&mut self, literal: &str, observed: (usize, usize)) -> (usize, usize) {
        if self.path.starts_with("khive-db/") {
            return (0, 0);
        }
        let (routes, unclassified) = sql_write_occurrences(literal);
        let counts = (
            routes.len() + unclassified.len(),
            runtime_table_write_count(literal),
        );
        let mut already_observed = observed.0;
        if let Some(target) = entity_first(&routes) {
            let class = sql_fixed_key_paths(literal)
                .map(detected_key_class)
                .unwrap_or(DetectedClass::WholeObject);
            let additional = routes.len().saturating_sub(already_observed);
            already_observed = already_observed.saturating_sub(routes.len());
            self.record(target, class, "SQL literal".into(), additional);
        }
        if let Some(target) = entity_first(&unclassified) {
            let additional = unclassified.len().saturating_sub(already_observed);
            self.record(
                target,
                DetectedClass::WholeObject,
                format!("{UNCLASSIFIED_SQL} {literal:?}"),
                additional,
            );
        }
        if counts.1 > 0 {
            let key = self.key();
            let calls = self.all_calls.get(&key).cloned().unwrap_or_default();
            let site = self
                .runtime_tables
                .entry(key.clone())
                .or_insert_with(|| RuntimeTableSite {
                    key,
                    write_count: 0,
                    calls,
                    evidence: BTreeSet::new(),
                });
            site.write_count += counts.1.saturating_sub(observed.1);
            site.evidence
                .insert(format!("runtime-table SQL {literal:?}"));
        }
        counts
    }
}

impl<'ast, 'modules> Visit<'ast> for SourceCollector<'modules> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if test_only(&item.attrs) {
            return;
        }
        let nested = static_sql_source::module_bindings(item, &self.loader_bindings, test_only);
        let outer_loaders = std::mem::replace(&mut self.loader_bindings, nested);
        let mut parents = self.parent_module_bindings.clone();
        parents.push(self.bindings.first().cloned().unwrap_or_default());
        let mut child_module = self.module_id.clone();
        child_module.segments.push(item.ident.to_string());
        let module_bindings = item
            .content
            .as_ref()
            .map_or_else(BTreeMap::new, |(_, items)| {
                use_bindings(
                    items.iter().filter_map(|item| match item {
                        syn::Item::Use(item) => Some(item),
                        _ => None,
                    }),
                    &parents,
                    None,
                    &child_module,
                    self.modules,
                    self.strict,
                )
            });
        let module_bindings = with_declared_types(module_bindings, &child_module, self.modules);
        let outer_bindings = std::mem::replace(&mut self.bindings, vec![module_bindings]);
        let outer_module = std::mem::replace(&mut self.module_id, child_module);
        self.parent_module_bindings
            .push(outer_bindings.first().cloned().unwrap_or_default());
        self.scope.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.scope.pop();
        self.parent_module_bindings.pop();
        self.bindings = outer_bindings;
        self.module_id = outer_module;
        self.loader_bindings = outer_loaders;
        self.loader_bindings.observe_module(item);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if !test_only(&item.attrs) {
            self.loader_bindings.observe_macro(item);
            syn::visit::visit_item_macro(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if test_only(&item.attrs) {
            return;
        }
        self.scope
            .push(item.self_ty.to_token_stream().to_string().replace(' ', ""));
        syn::visit::visit_item_impl(self, item);
        self.scope.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        self.bindings
            .push(shadow_bindings(item.sig.inputs.iter().filter_map(|arg| {
                if let FnArg::Typed(arg) = arg {
                    Some(&*arg.pat)
                } else {
                    None
                }
            })));
        self.scope.push(item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.scope.pop();
        self.bindings.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        self.bindings
            .push(shadow_bindings(item.sig.inputs.iter().filter_map(|arg| {
                if let FnArg::Typed(arg) = arg {
                    Some(&*arg.pat)
                } else {
                    None
                }
            })));
        self.scope.push(item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.scope.pop();
        self.bindings.pop();
    }

    fn visit_block(&mut self, block: &'ast Block) {
        let nested = static_sql_source::block_bindings(block, &self.loader_bindings, test_only);
        let outer_loaders = std::mem::replace(&mut self.loader_bindings, nested);
        let mut block_bindings = use_bindings(
            block.stmts.iter().filter_map(|stmt| {
                if let Stmt::Item(syn::Item::Use(item)) = stmt {
                    Some(item)
                } else {
                    None
                }
            }),
            &self.parent_module_bindings,
            self.bindings.first(),
            &self.module_id,
            self.modules,
            self.strict,
        );
        for stmt in &block.stmts {
            if let Stmt::Item(item) = stmt {
                block_bindings.extend(
                    declared_binding(item).filter(|(_, binding)| *binding == Binding::Type),
                );
            }
        }
        self.bindings.push(block_bindings);
        syn::visit::visit_block(self, block);
        self.bindings.pop();
        self.loader_bindings = outer_loaders;
    }

    fn visit_local(&mut self, local: &'ast Local) {
        syn::visit::visit_local(self, local);
        let shadows = shadow_bindings(std::iter::once(&local.pat));
        self.bindings
            .last_mut()
            .expect("local has a block scope")
            .extend(shadows);
    }

    fn visit_expr_closure(&mut self, expr: &'ast syn::ExprClosure) {
        self.bindings.push(shadow_bindings(expr.inputs.iter()));
        syn::visit::visit_expr_closure(self, expr);
        self.bindings.pop();
    }

    fn visit_expr_method_call(&mut self, expr: &'ast ExprMethodCall) {
        let name = expr.method.to_string();
        if !self.path.starts_with("khive-db/") && STORE_WRITES.contains(&name.as_str()) {
            let path_index = match name.as_str() {
                "set_note_property" => Some(1),
                "try_patch_note_property" | "patch_note_property_atomic" => Some(3),
                _ => None,
            };
            let class = path_index
                .and_then(|index| expr.args.iter().nth(index))
                .and_then(literal_string)
                .filter(|path| bare_top_level_path(path))
                .map(DetectedClass::SingleKey)
                .unwrap_or(DetectedClass::WholeObject);
            let target = if name.contains("entity") || name.contains("entities") {
                Substrate::Entity
            } else {
                Substrate::Note
            };
            self.record(target, class, format!("store.{name}"), 1);
        }
        self.call(&name);
        syn::visit::visit_expr_method_call(self, expr);
    }

    fn visit_expr_call(&mut self, expr: &'ast ExprCall) {
        if let Expr::Path(path) = &*expr.func {
            if let Some(last) = path.path.segments.last() {
                let name = last.ident.to_string();
                if !self.path.starts_with("khive-db/") {
                    let target = if ENTITY_BUILDERS.contains(&name.as_str()) {
                        Some(Substrate::Entity)
                    } else if NOTE_BUILDERS.contains(&name.as_str()) {
                        Some(Substrate::Note)
                    } else {
                        None
                    };
                    if let Some(target) = target {
                        let class = if name == "note_set_property_statement" {
                            expr.args
                                .iter()
                                .nth(1)
                                .and_then(literal_string)
                                .filter(|path| bare_top_level_path(path))
                                .map(DetectedClass::SingleKey)
                                .unwrap_or(DetectedClass::WholeObject)
                        } else {
                            DetectedClass::WholeObject
                        };
                        self.record(target, class, format!("builder.{name}"), 1);
                    }
                }
                self.call(&name);
            }
        }
        syn::visit::visit_expr_call(self, expr);
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        let ordinal = self.path_ordinal;
        self.path_ordinal += 1;
        if !self.path.starts_with("khive-db/") {
            if let Some(segment) = expr.path.segments.last() {
                let name = segment.ident.to_string();
                let segments = &expr.path.segments;
                let imported = if segments.len() == 1 {
                    self.bindings
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(&name))
                } else if segments.len() == 2 && segments[0].ident == "self" {
                    self.bindings.first().and_then(|scope| scope.get(&name))
                } else if segments.len() == 2 && segments[0].ident == "crate" {
                    self.parent_module_bindings
                        .first()
                        .or_else(|| self.bindings.first())
                        .and_then(|scope| scope.get(&name))
                } else {
                    let depth = segments.len() - 1;
                    if segments
                        .iter()
                        .take(depth)
                        .all(|segment| segment.ident == "super")
                    {
                        self.parent_module_bindings
                            .len()
                            .checked_sub(depth)
                            .and_then(|index| self.parent_module_bindings.get(index))
                            .and_then(|scope| scope.get(&name))
                    } else {
                        None
                    }
                };
                let binding = if let Some(binding) = imported {
                    Some(binding.clone())
                } else {
                    let path = segments
                        .iter()
                        .map(|part| part.ident.to_string())
                        .collect::<Vec<_>>();
                    qualified_target(
                        &path,
                        &self.module_id,
                        self.modules,
                        &|first| self.scoped_binding(first),
                        self.strict,
                    )
                };
                if let Some(Binding::Constant(constant)) = binding {
                    self.constant_references.insert(
                        ordinal,
                        SqlConstantReference {
                            site: self.key(),
                            constant: constant.clone(),
                        },
                    );
                    self.record(
                        Substrate::Note,
                        DetectedClass::WholeObject,
                        format!("{SQL_CONSTANT_EVIDENCE}{constant}"),
                        1,
                    );
                }
            }
        }
        syn::visit::visit_expr_path(self, expr);
    }

    fn visit_expr_lit(&mut self, expr: &'ast ExprLit) {
        if let Lit::Str(value) = &expr.lit {
            self.record_sql(&value.value(), (0, 0));
        }
        syn::visit::visit_expr_lit(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        match static_sql_source::resolve_static_sql(
            &self.path,
            mac,
            self.sql_sources,
            &self.loader_bindings,
        ) {
            Ok(Some(sql)) => {
                self.resolved_sql.insert(sql.asset_path);
                self.record_sql(&sql.produced_text, (0, 0));
                return;
            }
            Err(error) => {
                self.sql_errors.push(error);
                return;
            }
            Ok(None) => {}
        }
        if static_sql_source::opaque_sql_loader(mac) {
            self.sql_errors
                .push(format!("{}: uninspectable nested SQL loader", self.path));
            return;
        }
        // Visit vec! through its parsed expressions once. A preceding token
        // literal pass would count the same SQL literal a second time.
        if mac.path.is_ident("vec") {
            let tokens = &mac.tokens;
            let expression = syn::parse2::<Expr>(quote::quote!([#tokens]))
                .unwrap_or_else(|error| panic!("{}: invalid vec! body: {error}", self.key()));
            <Self as Visit<'_>>::visit_expr(self, &expression);
        } else {
            let mut parts = Vec::new();
            macro_strings(mac.tokens.clone(), &mut parts);
            let mut observed = (0, 0);
            for part in &parts {
                let counts = self.record_sql(part, (0, 0));
                observed.0 += counts.0;
                observed.1 += counts.1;
            }
            if mac.path.is_ident("format") || mac.path.is_ident("concat") {
                // Preserve joined-string detection, including split writes,
                // without counting its already observed literals twice.
                self.record_sql(&parts.concat(), observed);
            }
        }
        syn::visit::visit_macro(self, mac);
    }
}

/// The type that an item declares, or the value. A type lives in the module
/// namespace, so it hides a module or crate of the same name at the head of a
/// path, and a value never does.
fn declared_binding(item: &syn::Item) -> Option<(String, Binding)> {
    match item {
        syn::Item::Struct(ItemStruct { ident, attrs, .. })
        | syn::Item::Enum(ItemEnum { ident, attrs, .. })
        | syn::Item::Union(ItemUnion { ident, attrs, .. })
        | syn::Item::Trait(ItemTrait { ident, attrs, .. })
        | syn::Item::TraitAlias(ItemTraitAlias { ident, attrs, .. })
        | syn::Item::Type(ItemType { ident, attrs, .. })
            if !test_only(attrs) =>
        {
            Some((ident.to_string(), Binding::Type))
        }
        syn::Item::Fn(ItemFn { sig, attrs, .. }) if !test_only(attrs) => {
            Some((sig.ident.to_string(), Binding::Value))
        }
        syn::Item::Const(ItemConst { ident, attrs, .. })
        | syn::Item::Static(ItemStatic { ident, attrs, .. })
            if !test_only(attrs) =>
        {
            Some((ident.to_string(), Binding::Value))
        }
        _ => None,
    }
}

/// The types that a module declares join the names in scope in its own
/// source, so `Ty::NAME` in the module that declares `Ty` is the type's
/// associated item.
fn with_declared_types(
    mut bindings: SqlBindings,
    module_id: &ModuleId,
    modules: &ModuleBindings,
) -> SqlBindings {
    for (name, binding) in modules.get(module_id).into_iter().flatten() {
        if *binding == Binding::Type {
            bindings
                .entry(name.clone())
                .or_insert_with(|| binding.clone());
        }
    }
    bindings
}

fn module_parents(module_id: &ModuleId, modules: &ModuleBindings) -> Vec<SqlBindings> {
    (0..module_id.segments.len())
        .map(|depth| {
            modules
                .get(&ModuleId {
                    root: module_id.root.clone(),
                    segments: module_id.segments[..depth].to_vec(),
                })
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn sql_statements(sql: &str) -> Vec<String> {
    #[derive(Clone, Copy)]
    enum Mode {
        Sql,
        SingleQuote,
        DoubleQuote,
        LineComment,
        BlockComment,
    }

    let mut statements = Vec::new();
    let mut statement = String::new();
    let mut chars = sql.chars().peekable();
    let mut mode = Mode::Sql;
    while let Some(character) = chars.next() {
        match mode {
            Mode::Sql => match character {
                '-' if chars.peek() == Some(&'-') => {
                    chars.next();
                    statement.push(' ');
                    mode = Mode::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    statement.push(' ');
                    mode = Mode::BlockComment;
                }
                '\'' => {
                    statement.push(character);
                    mode = Mode::SingleQuote;
                }
                '"' => {
                    statement.push(character);
                    mode = Mode::DoubleQuote;
                }
                ';' => {
                    if !statement.trim().is_empty() {
                        statements.push(std::mem::take(&mut statement));
                    }
                }
                _ => statement.push(character),
            },
            Mode::SingleQuote | Mode::DoubleQuote => {
                statement.push(character);
                let quote = if matches!(mode, Mode::SingleQuote) {
                    '\''
                } else {
                    '"'
                };
                if character == quote {
                    if chars.peek() == Some(&quote) {
                        statement.push(chars.next().expect("peeked quote"));
                    } else {
                        mode = Mode::Sql;
                    }
                }
            }
            Mode::LineComment => {
                if character == '\n' {
                    statement.push(' ');
                    mode = Mode::Sql;
                }
            }
            Mode::BlockComment => {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    statement.push(' ');
                    mode = Mode::Sql;
                }
            }
        }
    }
    if !statement.trim().is_empty() {
        statements.push(statement);
    }
    statements
}

fn scan_migration_sources(sources: &[(String, String)]) -> Vec<Site> {
    let mut sites = Vec::new();
    for (path, sql) in sources {
        for (index, statement) in sql_statements(sql).iter().enumerate() {
            let unclassified = sql_unclassified_write(statement);
            if let Some(target) = sql_target(statement).or(unclassified) {
                let mut evidence = BTreeSet::<String>::from(["migration SQL".into()]);
                if unclassified.is_some() {
                    evidence.insert(format!("{UNCLASSIFIED_SQL} {statement:?}"));
                }
                sites.push(Site {
                    key: format!("{path}::statement_{}", index + 1),
                    target,
                    route_class: RouteClass::Migration,
                    class: DetectedClass::WholeObject,
                    write_count: 1,
                    calls: BTreeSet::new(),
                    evidence,
                });
            }
        }
    }
    sites.sort_by(|a, b| a.key.cmp(&b.key));
    sites
}

fn source_path(path: &Path) -> Option<String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                segments.pop()?;
            }
            _ => return None,
        }
    }
    Some(segments.join("/"))
}

fn module_source_paths(
    parent: &str,
    inline_dirs: &[String],
    module: &ItemMod,
    sources: &BTreeSet<String>,
) -> Vec<String> {
    let parent_path = Path::new(parent);
    let parent_dir = parent_path.parent().unwrap_or_else(|| Path::new(""));
    let override_path = module
        .attrs
        .iter()
        .find(|attr| attr.path().is_ident("path"));
    let candidates = if let Some(attr) = override_path {
        let syn::Meta::NameValue(value) = &attr.meta else {
            return Vec::new();
        };
        let Some(name) = literal_string(&value.value) else {
            return Vec::new();
        };
        let mut base = parent_dir.to_path_buf();
        for segment in inline_dirs {
            base.push(segment);
        }
        vec![base.join(name)]
    } else {
        let mut base = parent_dir.to_path_buf();
        let filename = parent_path.file_name().and_then(|name| name.to_str());
        if !matches!(filename, Some("lib.rs" | "main.rs" | "mod.rs")) {
            if let Some(stem) = parent_path.file_stem() {
                base.push(stem);
            }
        }
        for segment in inline_dirs {
            base.push(segment);
        }
        let module_base = base.join(module.ident.to_string());
        vec![module_base.with_extension("rs"), module_base.join("mod.rs")]
    };
    candidates
        .into_iter()
        .filter_map(|candidate| source_path(&candidate))
        .filter(|child| sources.contains(child))
        .collect()
}

fn source_inclusions(
    parent: &str,
    items: &[syn::Item],
    inline_dirs: &mut Vec<String>,
    inherited_test_only: bool,
    sources: &BTreeSet<String>,
    edges: &mut BTreeMap<String, Vec<(String, bool)>>,
    incoming: &mut BTreeSet<String>,
) {
    let parent_path = Path::new(parent);
    let parent_dir = parent_path.parent().unwrap_or_else(|| Path::new(""));
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                let only_test = inherited_test_only || test_only(&module.attrs);
                if let Some((_, nested)) = &module.content {
                    inline_dirs.push(module.ident.to_string());
                    source_inclusions(
                        parent,
                        nested,
                        inline_dirs,
                        only_test,
                        sources,
                        edges,
                        incoming,
                    );
                    inline_dirs.pop();
                    continue;
                }

                for child in module_source_paths(parent, inline_dirs, module, sources) {
                    incoming.insert(child.clone());
                    edges
                        .entry(parent.to_owned())
                        .or_default()
                        .push((child, only_test));
                }
            }
            syn::Item::Macro(item) if item.mac.path.is_ident("include") => {
                let Ok(name) = syn::parse2::<syn::LitStr>(item.mac.tokens.clone()) else {
                    continue;
                };
                let Some(child) = source_path(&parent_dir.join(name.value())) else {
                    continue;
                };
                if sources.contains(&child) {
                    incoming.insert(child.clone());
                    edges
                        .entry(parent.to_owned())
                        .or_default()
                        .push((child, inherited_test_only || test_only(&item.attrs)));
                }
            }
            _ => {}
        }
    }
}

fn test_module_files(
    sources: &[(String, String)],
) -> Result<(BTreeSet<String>, BTreeSet<String>), syn::Error> {
    let paths = sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let mut edges = BTreeMap::new();
    let mut incoming = BTreeSet::new();
    for (path, source) in sources {
        let file = syn::parse_file(source)?;
        source_inclusions(
            path,
            &file.items,
            &mut Vec::new(),
            false,
            &paths,
            &mut edges,
            &mut incoming,
        );
    }

    let mut roots = paths
        .difference(&incoming)
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in &paths {
        if path.split_once("/src/").is_some_and(|(_, source)| {
            source == "lib.rs"
                || source == "main.rs"
                || source.strip_prefix("bin/").is_some_and(|binary| {
                    !binary.contains('/')
                        || (binary.ends_with("/main.rs") && binary.matches('/').count() == 1)
                })
        }) {
            roots.insert(path.clone());
        }
    }
    let mut production = roots.clone();
    for path in &paths {
        if path
            .split_once("/src/")
            .is_some_and(|(_, source)| source.starts_with("bin/"))
        {
            production.insert(path.clone());
        }
    }
    let mut pending = production.iter().cloned().collect::<Vec<_>>();
    while let Some(parent) = pending.pop() {
        if let Some(children) = edges.get(&parent) {
            for (child, only_test) in children {
                if !only_test && production.insert(child.clone()) {
                    pending.push(child.clone());
                }
            }
        }
    }
    Ok((incoming.difference(&production).cloned().collect(), roots))
}

fn scan_source_population(sources: &[(String, String)]) -> Result<SourcePopulation, String> {
    scan_source_population_with_sql(sources, &StaticSqlSources::new())
}

fn scan_source_population_with_sql(
    sources: &[(String, String)],
    sql_sources: &StaticSqlSources,
) -> Result<SourcePopulation, String> {
    let (skipped, roots) = test_module_files(sources).map_err(|error| error.to_string())?;
    let parsed = parse_production_sources(sources, &skipped)?;
    let loader_bindings = static_sql_source::canonical_bindings(&parsed, test_only);
    // The census resolves every path twice. The strict resolution stops at a
    // name that it cannot classify as a module, and the lenient one looks past
    // it. A path that only the lenient resolution takes to a note SQL constant
    // would be a route if the name were a function and no route if it were a
    // type, so the census refuses it instead of choosing.
    let mut strict = BTreeMap::new();
    let mut strict_references = BTreeMap::new();
    let mut runtime_tables = BTreeMap::new();
    let mut lenient = Vec::new();
    let mut resolved_sql = BTreeSet::new();
    for is_strict in [true, false] {
        let (modules, file_modules) = index_module_bindings(&parsed, &roots, is_strict);
        for (path, file) in &parsed {
            let module_id = file_modules
                .get(path)
                .cloned()
                .expect("indexed production source");
            let scanned = scan_source(
                path,
                file,
                module_id,
                &modules,
                is_strict,
                sql_sources,
                loader_bindings[path].clone(),
            );
            if !scanned.sql_errors.is_empty() {
                return Err(scanned.sql_errors.join("\n"));
            }
            if is_strict {
                resolved_sql.extend(scanned.resolved_sql);
                runtime_tables.extend(
                    scanned
                        .runtime_tables
                        .into_iter()
                        .map(|site| (site.key.clone(), site)),
                );
                strict.extend(
                    scanned
                        .sites
                        .into_iter()
                        .map(|site| (site.key.clone(), site)),
                );
                strict_references.extend(
                    scanned
                        .constant_references
                        .into_iter()
                        .map(|(ordinal, reference)| ((path.clone(), ordinal), reference)),
                );
            } else {
                lenient.push((path, scanned));
            }
        }
    }
    for (path, scanned) in lenient {
        let sites = scanned
            .sites
            .into_iter()
            .map(|site| (site.key.clone(), site))
            .collect::<BTreeMap<_, _>>();
        for (ordinal, reference) in scanned.constant_references {
            if strict_references.get(&(path.clone(), ordinal)) == Some(&reference) {
                continue;
            }
            strict
                .entry(reference.site.clone())
                .or_insert_with(|| Site {
                    evidence: BTreeSet::new(),
                    ..sites
                        .get(&reference.site)
                        .expect("constant reference has a site")
                        .clone()
                })
                .evidence
                .insert(format!("{UNRESOLVED_CONSTANT} {}", reference.constant));
        }
    }
    // Maps keyed by the site key are already in key order.
    Ok(SourcePopulation {
        properties: strict.into_values().collect(),
        runtime_tables: runtime_tables.into_values().collect(),
        resolved_sql,
    })
}

/// A guarded write in a SQL asset is a route only through the call site that
/// loads it, and the census sees that site only when it follows the loader.
/// An asset holding such a write that no followed loader reached (a re-exported
/// or renamed `sql!`, `include_bytes!`, a `concat!` path, a wrapper macro) is
/// refused here instead of passing unseen. Not covered: a second, unfollowed
/// load of an asset that a followed loader also reaches.
fn check_sql_asset_reach(
    sql_sources: &StaticSqlSources,
    resolved: &BTreeSet<String>,
) -> Result<(), String> {
    let unreached = sql_sources
        .iter()
        .filter(|(path, _)| !path.starts_with("khive-db/") && !resolved.contains(*path))
        .filter(|(_, sql)| {
            let (routes, unclassified) = sql_write_occurrences(sql);
            !routes.is_empty() || !unclassified.is_empty() || runtime_table_write_count(sql) > 0
        })
        .map(|(path, _)| format!("{path}: guarded SQL write not reached by a followed loader"))
        .collect::<Vec<_>>();
    if unreached.is_empty() {
        Ok(())
    } else {
        Err(unreached.join("\n"))
    }
}

fn runtime_table_routes(
    population: &SourcePopulation,
    inventory: &[RouteInventoryEntry],
    runtime_inventory: &[RuntimeTableWriteInventoryEntry],
    require_all: bool,
) -> Result<Vec<Site>, String> {
    let mut failures = Vec::new();
    let mut declarations = BTreeMap::new();
    for row in runtime_inventory {
        if declarations.insert(row.site, row).is_some() {
            failures.push(format!("duplicate runtime-table write site {}", row.site));
        }
        if row.expected_writes == 0 {
            failures.push(format!(
                "{}: runtime-table expected write count must be nonzero",
                row.site
            ));
        }
        if let Some(id) = row.properties_route {
            if !inventory
                .iter()
                .any(|route| route.id == id && route.site == row.site)
            {
                failures.push(format!(
                    "{}: runtime-table properties route {id} is absent or names another site",
                    row.site
                ));
            }
        }
    }
    let mut properties = population
        .properties
        .iter()
        .cloned()
        .map(|site| (site.key.clone(), site))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for site in &population.runtime_tables {
        seen.insert(site.key.as_str());
        let Some(row) = declarations.get(site.key.as_str()) else {
            failures.push(format!(
                "unmapped runtime-table write {}: {:?}",
                site.key, site.evidence
            ));
            continue;
        };
        if site.write_count != row.expected_writes {
            failures.push(format!(
                "{}: runtime-table write count {} differs from declared {}",
                site.key, site.write_count, row.expected_writes
            ));
        }
        if let Some(id) = row.properties_route {
            let Some(route) = inventory
                .iter()
                .find(|route| route.id == id && route.site == row.site)
            else {
                continue;
            };
            let combined = properties.entry(site.key.clone()).or_insert_with(|| Site {
                key: site.key.clone(),
                target: route.target,
                route_class: RouteClass::Application,
                class: DetectedClass::WholeObject,
                write_count: 0,
                calls: BTreeSet::new(),
                evidence: BTreeSet::new(),
            });
            if combined.target != route.target {
                combined
                    .evidence
                    .insert("MIXED_ENTITY_AND_NOTE_TARGETS".into());
            }
            combined.class = DetectedClass::WholeObject;
            combined.write_count += site.write_count;
            combined.calls.extend(site.calls.iter().cloned());
            combined.evidence.extend(site.evidence.iter().cloned());
        }
    }
    if require_all {
        for row in runtime_inventory {
            if !seen.contains(row.site) {
                failures.push(format!("orphan runtime-table write at {}", row.site));
            }
        }
    }
    if failures.is_empty() {
        Ok(properties.into_values().collect())
    } else {
        Err(failures.join("\n"))
    }
}

fn scan_sources(sources: &[(String, String)]) -> Result<Vec<Site>, String> {
    let population = scan_source_population(sources)?;
    runtime_table_routes(
        &population,
        ROUTE_INVENTORY,
        RUNTIME_TABLE_WRITE_INVENTORY,
        false,
    )
}

fn check_population(
    population: &SourcePopulation,
    inventory: &[RouteInventoryEntry],
    runtime_inventory: &[RuntimeTableWriteInventoryEntry],
    pinned_missing: usize,
) -> Result<(), String> {
    let sites = runtime_table_routes(population, inventory, runtime_inventory, true)?;
    check_inventory(&sites, inventory, pinned_missing)
}

fn check_inventory(
    sites: &[Site],
    inventory: &[RouteInventoryEntry],
    pinned_missing: usize,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut rows = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for row in inventory {
        if !ids.insert(row.id) {
            failures.push(format!("duplicate route id {}", row.id));
        }
        if rows.insert(row.site, row).is_some() {
            failures.push(format!("duplicate route site {}", row.site));
        }
        if row.expected_writes == 0 {
            failures.push(format!(
                "{}: expected write count must be nonzero",
                row.site
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for site in sites {
        seen.insert(site.key.as_str());
        for evidence in site
            .evidence
            .iter()
            .filter(|evidence| evidence.starts_with(UNRESOLVED_CONSTANT))
        {
            failures.push(format!(
                "{}: cannot tell whether this site reaches a note SQL constant ({evidence})",
                site.key
            ));
        }
        for evidence in site
            .evidence
            .iter()
            .filter(|evidence| evidence.starts_with(UNCLASSIFIED_SQL))
        {
            failures.push(format!(
                "{}: cannot classify this write to a guarded table ({evidence})",
                site.key
            ));
        }
        let Some(row) = rows.get(site.key.as_str()) else {
            failures.push(format!("unmapped {}: {:?}", site.key, site.evidence));
            continue;
        };
        if site.write_count != row.expected_writes {
            failures.push(format!(
                "{}: write count {} differs from declared {}",
                site.key, site.write_count, row.expected_writes
            ));
        }
        if site.evidence.contains("MIXED_ENTITY_AND_NOTE_TARGETS") || site.target != row.target {
            failures.push(format!(
                "{}: target does not describe every write",
                site.key
            ));
        }
        let declared_class = if row.transaction == TransactionOwner::Migration {
            RouteClass::Migration
        } else {
            RouteClass::Application
        };
        if site.route_class != declared_class {
            failures.push(format!("{}: route class disagrees with source", site.key));
        }
        if site.route_class == RouteClass::Migration
            && (row.write_class != WriteClass::PrivilegedEscape
                || row.reservation != Reservation::PrivilegedEscape
                || row.transaction != TransactionOwner::Migration)
        {
            failures.push(format!(
                "{}: migration requires an explicit privileged escape",
                site.key
            ));
        }
        match (&row.write_class, &site.class) {
            (WriteClass::SingleKey { key_path }, DetectedClass::SingleKey(actual))
                if canonical_key_path(key_path).is_some()
                    && canonical_key_path(key_path) == canonical_key_path(actual) =>
            {
                if row.reservation != Reservation::ByConstruction {
                    failures.push(format!(
                        "{}: single-key route must be reserved by construction",
                        site.key
                    ));
                }
            }
            (WriteClass::FixedKeySet { key_paths }, detected)
                if !key_paths.is_empty()
                    && detected_keys(detected).is_some()
                    && key_paths
                        .iter()
                        .map(|path| canonical_key_path(path))
                        .collect::<Option<BTreeSet<_>>>()
                        == detected_keys(detected) =>
            {
                if row.reservation != Reservation::ByConstruction {
                    failures.push(format!(
                        "{}: fixed-key-set route must be reserved by construction",
                        site.key
                    ));
                }
            }
            (
                WriteClass::WholeObject,
                DetectedClass::WholeObject | DetectedClass::FixedKeySet(_),
            ) => match row.reservation {
                Reservation::NamedCheck { function, .. } if site.calls.contains(function) => {}
                _ => failures.push(format!(
                    "{}: whole-object write lacks its named check/callee",
                    site.key
                )),
            },
            (WriteClass::PrivilegedEscape, _)
                if row.reservation == Reservation::PrivilegedEscape => {}
            _ => failures.push(format!(
                "{}: route write class disagrees with source ({:?})",
                site.key, site.class
            )),
        }
    }
    for row in inventory {
        if !seen.contains(row.site) {
            failures.push(format!("orphan route {} at {}", row.id, row.site));
        }
    }
    let missing = inventory
        .iter()
        .filter(|row| row.acceptance == Acceptance::Missing)
        .count();
    if missing != pinned_missing {
        failures.push(format!(
            "Missing acceptance count {missing} differs from pinned {pinned_missing}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn check_store_trait_methods(sources: &[(String, String)]) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut observed = BTreeSet::new();
    let mut traits_seen = BTreeSet::new();
    for (path, source) in sources {
        if path != "khive-storage/src/entity.rs" && path != "khive-storage/src/note.rs" {
            continue;
        }
        let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
        for item in file.items {
            let syn::Item::Trait(trait_item) = item else {
                continue;
            };
            if trait_item.ident != "EntityStore" && trait_item.ident != "NoteStore" {
                continue;
            }
            traits_seen.insert(trait_item.ident.to_string());
            for member in trait_item.items {
                let syn::TraitItem::Fn(method) = member else {
                    continue;
                };
                let name = method.sig.ident.to_string();
                if !STORE_WRITES.contains(&name.as_str())
                    && !NON_PROPERTIES_STORE_METHODS.contains(&name.as_str())
                {
                    failures.push(format!("{path}: unclassified store method {name}"));
                }
                observed.insert(name);
            }
        }
    }
    for trait_name in ["EntityStore", "NoteStore"] {
        if !traits_seen.contains(trait_name) {
            failures.push(format!("missing store trait {trait_name}"));
        }
    }
    for name in STORE_WRITES {
        if NON_PROPERTIES_STORE_METHODS.contains(name) {
            failures.push(format!("store method {name} has two classifications"));
        }
    }
    for name in STORE_WRITES
        .iter()
        .chain(NON_PROPERTIES_STORE_METHODS.iter())
    {
        if !observed.contains(*name) {
            failures.push(format!(
                "classified store method {name} is absent from traits"
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn source_files(dir: &Path, root: &Path, extension: &str, output: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
    {
        let entry = entry.expect("source entry");
        let path = entry.path();
        if entry.file_type().expect("source file type").is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "tests" || name == "benches")
            {
                continue;
            }
            source_files(&path, root, extension, output);
        } else if path.extension().is_some_and(|actual| actual == extension) {
            let relative = path
                .strip_prefix(root)
                .expect("workspace-relative path")
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            output.push((relative, source));
        }
    }
}

fn live_workspace_sql() -> StaticSqlSources {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf();
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates directory") {
        let sql = entry.expect("crate directory").path().join("sql");
        if sql.is_dir() {
            source_files(&sql, &crates, "sql", &mut sources);
        }
    }
    sources.into_iter().collect()
}

fn live_workspace_sources() -> Vec<(String, String)> {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf();
    let manifest =
        std::fs::read_to_string(crates.join("Cargo.toml")).expect("read workspace manifest");
    let members = manifest
        .split_once("members = [")
        .expect("workspace members")
        .1
        .split_once(']')
        .expect("closed workspace members")
        .0;
    let mut sources = Vec::new();
    for member in members.lines() {
        let member = member.trim().trim_end_matches(',').trim_matches('"');
        if member.is_empty() || member.starts_with('#') {
            continue;
        }
        let src = crates.join(member).join("src");
        if src.exists() {
            source_files(&src, &crates, "rs", &mut sources);
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

#[path = "route_census_migrations.rs"]
mod migrations;
use migrations::live_migration_sources;

#[cfg(test)]
#[path = "route_census_occurrence_tests.rs"]
mod route_census_occurrence_tests;

#[cfg(test)]
#[path = "route_census_declaration_tests.rs"]
mod route_census_declaration_tests;

#[cfg(test)]
#[path = "route_census_parsed_sources_tests.rs"]
mod parsed_sources_tests;

#[cfg(test)]
#[path = "route_census_synthetic_controls_tests.rs"]
mod route_census_synthetic_controls_tests;

#[cfg(test)]
#[path = "route_census_static_sql_tests.rs"]
mod route_census_static_sql_tests;

#[cfg(test)]
#[path = "route_census_shadowing_tests.rs"]
mod shadowing_tests;

#[test]
fn source_census_matches_closed_route_inventory() {
    let sources = live_workspace_sources();
    check_store_trait_methods(&sources).expect("store method surface drifted");
    let sql_sources = live_workspace_sql();
    let mut population =
        scan_source_population_with_sql(&sources, &sql_sources).expect("parse workspace sources");
    check_sql_asset_reach(&sql_sources, &population.resolved_sql)
        .unwrap_or_else(|failure| panic!("ADR-115 route census failed:\n{failure}"));
    population
        .properties
        .extend(scan_migration_sources(&live_migration_sources()));
    population.properties.sort_by(|a, b| a.key.cmp(&b.key));
    for site in &population.properties {
        eprintln!(
            "ROUTE SITE | {} | count={} | {:?} | {:?} | {:?}",
            site.key, site.write_count, site.target, site.class, site.evidence
        );
    }
    for site in &population.runtime_tables {
        eprintln!(
            "RUNTIME TABLE SITE | {} | count={} | {:?}",
            site.key, site.write_count, site.evidence
        );
    }
    check_population(
        &population,
        ROUTE_INVENTORY,
        RUNTIME_TABLE_WRITE_INVENTORY,
        PINNED_MISSING_ACCEPTANCE,
    )
    .unwrap_or_else(|failure| panic!("ADR-115 route census failed:\n{failure}"));
}

#[test]
fn migration_sql_is_inventoried() {
    let sites = scan_migration_sources(&live_migration_sources());
    assert!(sites.iter().any(|site| {
        site.key == "khive-db/sql/005-unique-comm-external-id.sql::statement_1"
            && site.target == Substrate::Note
            && site.route_class == RouteClass::Migration
    }));
    let fixture = scan_migration_sources(&[(
        "khive-db/sql/999-census-fixture.sql".into(),
        "-- UPDATE entities SET properties is only a comment\nUPDATE notes SET properties = '{}' WHERE id = 'fixture';".into(),
    )]);
    assert_eq!(fixture.len(), 1);
    assert_eq!(fixture[0].route_class, RouteClass::Migration);
    assert!(check_inventory(&fixture, &[], 0)
        .unwrap_err()
        .contains("unmapped khive-db/sql/999-census-fixture.sql::statement_1"));
}

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
