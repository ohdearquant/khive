//! Source-side guard for ADR-115 Amendment 5's properties write population.
//! The scanner is pure over `(path, text)` pairs so mutations can exercise it
//! without building or loading another crate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;
use syn::{
    Attribute, Expr, ExprCall, ExprLit, ExprMethodCall, ImplItemFn, ItemFn, ItemImpl, ItemMod, Lit,
    Macro,
};

use super::declaration::{
    Acceptance, Reservation, RouteInventoryEntry, Substrate, TransactionOwner, WriteClass,
    PINNED_MISSING_ACCEPTANCE, ROUTE_INVENTORY,
};

const STORE_WRITES: &[&str] = &[
    "upsert_entity",
    "upsert_entities",
    "upsert_entity_with_attachments",
    "insert_entity_if_absent",
    "replace_entity_if_unchanged",
    "upsert_note",
    "upsert_notes",
    "insert_note_if_absent",
    "try_insert_note",
    "try_insert_note_with_attachments",
    "replace_note_if_unchanged",
    "update_note_properties",
    "set_note_property",
    "try_patch_note_property",
    "patch_note_property_atomic",
];

// The complement is explicit, including readers: a new trait method of any
// spelling makes the census red until its properties behavior is classified.
const NON_PROPERTIES_STORE_METHODS: &[&str] = &[
    "get_entity",
    "delete_entity",
    "query_entities",
    "entity_sequence",
    "query_entities_after",
    "count_entities",
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum DetectedClass {
    WholeObject,
    SingleKey(String),
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
    calls: BTreeSet<String>,
    evidence: BTreeSet<String>,
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

fn sql_target(literal: &str) -> Option<Substrate> {
    // Match SQL tokens rather than keeping SQL-shaped matcher literals in this
    // census: other source censuses must not mistake those for writer sites.
    let insert = "INSERT";
    let update = "UPDATE";
    let into = "INTO";
    let or = "OR";
    let ignore = "IGNORE";
    let replace = "REPLACE";
    let set = "SET";
    let where_token = "WHERE";
    let properties = "PROPERTIES";
    let normalized = literal.to_ascii_uppercase();
    let words = normalized
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    for (table, target) in [("ENTITIES", Substrate::Entity), ("NOTES", Substrate::Note)] {
        let plain_insert = words
            .windows(3)
            .any(|window| window[0] == insert && window[1] == into && window[2] == table);
        let conflict_insert = words.windows(5).any(|window| {
            window[0] == insert
                && window[1] == or
                && (window[2] == ignore || window[2] == replace)
                && window[3] == into
                && window[4] == table
        });
        let plain_replace = words
            .windows(3)
            .any(|window| window[0] == replace && window[1] == into && window[2] == table);
        let properties_update = words.windows(3).enumerate().any(|(index, window)| {
            window[0] == update
                && window[1] == table
                && window[2] == set
                && words[index + 3..]
                    .split(|word| *word == where_token)
                    .next()
                    .is_some_and(|set_clause| set_clause.contains(&properties))
        });
        if plain_insert || conflict_insert || plain_replace || properties_update {
            return Some(target);
        }
    }
    None
}

/// Conservative SQL classification: only a single fixed, bare top-level
/// `json_set` path in the SET clause can be reserved by construction. WHERE
/// predicates may read other paths and do not change this classification.
fn sql_single_key_path(literal: &str) -> Option<String> {
    let normalized = literal
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let set = normalized.split_once("set properties = json_set(")?.1;
    let set = set.split(" where ").next()?;
    if set.contains("json_remove(")
        || set.contains("json_patch(")
        || set.contains("json_insert(")
        || set.contains("json_replace(")
        || set.contains("json_set(")
    {
        return None;
    }
    let paths = set
        .split('\'')
        .filter(|piece| piece.starts_with('$'))
        .collect::<Vec<_>>();
    if paths.len() == 1 && bare_top_level_path(paths[0]) {
        Some(paths[0].to_owned())
    } else {
        None
    }
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

struct SourceCollector {
    path: String,
    scope: Vec<String>,
    sites: BTreeMap<String, Site>,
    all_calls: BTreeMap<String, BTreeSet<String>>,
}

impl SourceCollector {
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
    }

    fn record(&mut self, target: Substrate, class: DetectedClass, evidence: String) {
        let key = self.key();
        let calls = self.all_calls.get(&key).cloned().unwrap_or_default();
        let site = self.sites.entry(key.clone()).or_insert_with(|| Site {
            key: key.clone(),
            target,
            route_class: RouteClass::Application,
            class: class.clone(),
            calls,
            evidence: BTreeSet::new(),
        });
        // A single function that writes both substrates cannot be described
        // by the current target enum. Keep both observations visible rather
        // than silently selecting the first one.
        if site.target != target {
            site.evidence.insert("MIXED_ENTITY_AND_NOTE_TARGETS".into());
        }
        if site.class != class {
            site.class = DetectedClass::WholeObject;
        }
        site.evidence.insert(evidence);
    }

    fn record_sql(&mut self, literal: &str) {
        if self.path.starts_with("khive-db/") {
            return;
        }
        if let Some(target) = sql_target(literal) {
            let class = sql_single_key_path(literal)
                .map(DetectedClass::SingleKey)
                .unwrap_or(DetectedClass::WholeObject);
            self.record(target, class, "SQL literal".into());
        }
    }
}

impl<'ast> Visit<'ast> for SourceCollector {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if test_only(&item.attrs) {
            return;
        }
        self.scope.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.scope.pop();
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
        self.scope.push(item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        self.scope.push(item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.scope.pop();
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
            self.record(target, class, format!("store.{name}"));
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
                        self.record(target, class, format!("builder.{name}"));
                    }
                }
                self.call(&name);
            }
        }
        syn::visit::visit_expr_call(self, expr);
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        if !self.path.starts_with("khive-db/") {
            if let Some(segment) = expr.path.segments.last() {
                let name = segment.ident.to_string();
                if NOTE_PROPERTY_SQL_CONSTANTS.contains(&name.as_str()) {
                    self.record(
                        Substrate::Note,
                        DetectedClass::WholeObject,
                        format!("SQL constant {name}"),
                    );
                }
            }
        }
        syn::visit::visit_expr_path(self, expr);
    }

    fn visit_expr_lit(&mut self, expr: &'ast ExprLit) {
        if let Lit::Str(value) = &expr.lit {
            self.record_sql(&value.value());
        }
        syn::visit::visit_expr_lit(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        let mut parts = Vec::new();
        macro_strings(mac.tokens.clone(), &mut parts);
        for part in &parts {
            self.record_sql(part);
        }
        if mac.path.is_ident("format") || mac.path.is_ident("concat") {
            self.record_sql(&parts.concat());
        }
        // syn visits a macro's path, not its token body. The atomic planners
        // build PlanStatements inside vec!, so parse that standard expression
        // syntax and visit its calls rather than silently orphaning routes.
        if mac.path.is_ident("vec") {
            let tokens = &mac.tokens;
            let expression = syn::parse2::<Expr>(quote::quote!([#tokens]))
                .unwrap_or_else(|error| panic!("{}: invalid vec! body: {error}", self.key()));
            <Self as Visit<'_>>::visit_expr(self, &expression);
        }
        syn::visit::visit_macro(self, mac);
    }
}

fn scan_source(path: &str, source: &str) -> Result<Vec<Site>, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut collector = SourceCollector {
        path: path.to_owned(),
        scope: Vec::new(),
        sites: BTreeMap::new(),
        all_calls: BTreeMap::new(),
    };
    collector.visit_file(&file);
    Ok(collector.sites.into_values().collect())
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
            if let Some(target) = sql_target(statement) {
                sites.push(Site {
                    key: format!("{path}::statement_{}", index + 1),
                    target,
                    route_class: RouteClass::Migration,
                    class: DetectedClass::WholeObject,
                    calls: BTreeSet::new(),
                    evidence: BTreeSet::from(["migration SQL".into()]),
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

                let override_path = module
                    .attrs
                    .iter()
                    .find(|attr| attr.path().is_ident("path"));
                let candidates = if let Some(attr) = override_path {
                    let syn::Meta::NameValue(value) = &attr.meta else {
                        continue;
                    };
                    let Some(name) = literal_string(&value.value) else {
                        continue;
                    };
                    let mut base = parent_dir.to_path_buf();
                    for segment in inline_dirs.iter() {
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
                    for segment in inline_dirs.iter() {
                        base.push(segment);
                    }
                    let module_base = base.join(module.ident.to_string());
                    vec![module_base.with_extension("rs"), module_base.join("mod.rs")]
                };
                for candidate in candidates {
                    let Some(child) = source_path(&candidate) else {
                        continue;
                    };
                    if sources.contains(&child) {
                        incoming.insert(child.clone());
                        edges
                            .entry(parent.to_owned())
                            .or_default()
                            .push((child, only_test));
                    }
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

fn test_module_files(sources: &[(String, String)]) -> Result<BTreeSet<String>, syn::Error> {
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

    let mut production = paths
        .difference(&incoming)
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in &paths {
        if path.split_once("/src/").is_some_and(|(_, source)| {
            source == "lib.rs" || source == "main.rs" || source.starts_with("bin/")
        }) {
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
    Ok(incoming.difference(&production).cloned().collect())
}

fn scan_sources(sources: &[(String, String)]) -> Result<Vec<Site>, String> {
    let skipped = test_module_files(sources).map_err(|error| error.to_string())?;
    let mut all = Vec::new();
    for (path, source) in sources {
        if skipped.contains(path) || path.contains("/tests/") || path.contains("/benches/") {
            continue;
        }
        all.extend(scan_source(path, source).map_err(|error| format!("{path}: {error}"))?);
    }
    all.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(all)
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
    }
    let mut seen = BTreeSet::new();
    for site in sites {
        seen.insert(site.key.as_str());
        let Some(row) = rows.get(site.key.as_str()) else {
            failures.push(format!("unmapped {}: {:?}", site.key, site.evidence));
            continue;
        };
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
                if *key_path == actual.as_str() =>
            {
                if row.reservation != Reservation::ByConstruction {
                    failures.push(format!(
                        "{}: single-key route must be reserved by construction",
                        site.key
                    ));
                }
            }
            (WriteClass::WholeObject, DetectedClass::WholeObject) => match row.reservation {
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

fn source_files(dir: &Path, root: &Path, output: &mut Vec<(String, String)>) {
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
            source_files(&path, root, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
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
            source_files(&src, &crates, &mut sources);
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

fn live_migration_sources() -> Vec<(String, String)> {
    let sql_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .join("khive-db/sql");
    let mut sources = Vec::new();
    for entry in
        std::fs::read_dir(&sql_dir).unwrap_or_else(|error| panic!("{}: {error}", sql_dir.display()))
    {
        let path = entry.expect("SQL source entry").path();
        if path.extension().is_some_and(|extension| extension == "sql") {
            let name = path.file_name().expect("SQL source name").to_string_lossy();
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            sources.push((format!("khive-db/sql/{name}"), source));
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

#[test]
fn source_census_matches_closed_route_inventory() {
    let sources = live_workspace_sources();
    check_store_trait_methods(&sources).expect("store method surface drifted");
    let mut sites = scan_sources(&sources).expect("parse workspace sources");
    sites.extend(scan_migration_sources(&live_migration_sources()));
    sites.sort_by(|a, b| a.key.cmp(&b.key));
    for site in &sites {
        eprintln!(
            "ROUTE SITE | {} | {:?} | {:?} | {:?}",
            site.key, site.target, site.class, site.evidence
        );
    }
    check_inventory(&sites, ROUTE_INVENTORY, PINNED_MISSING_ACCEPTANCE)
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
    let source_path = "khive-runtime/src/curation.rs";
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
        .contains("unmapped khive-runtime/src/curation.rs::merge_note_sql"));

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
fn synthetic_census_controls() {
    let vector_sites = scan_sources(&[(
        "sample/src/lib.rs".into(),
        "fn entity() { let _ = vec![PlanStatement { statement: entity_upsert_statement(&value) }]; } fn note() { let _ = vec![note_upsert_statement(&value); 2]; }".into(),
    )])
    .unwrap();
    assert_eq!(vector_sites.len(), 2);
    assert!(vector_sites.iter().any(|site| {
        site.key == "sample/src/lib.rs::entity"
            && site.evidence.contains("builder.entity_upsert_statement")
    }));
    assert!(vector_sites.iter().any(|site| {
        site.key == "sample/src/lib.rs::note"
            && site.evidence.contains("builder.note_upsert_statement")
    }));
    assert!(check_inventory(&vector_sites, &[], 0)
        .unwrap_err()
        .contains("unmapped"));

    let whole = scan_sources(&[(
        "sample/src/lib.rs".into(),
        "fn write(store: &dyn NoteStore, note: Note) { store.upsert_note(note); }".into(),
    )])
    .unwrap();
    assert!(check_inventory(&whole, &[], 0)
        .unwrap_err()
        .contains("unmapped"));
    assert!(check_inventory(&[], &ROUTE_INVENTORY[..1], 1)
        .unwrap_err()
        .contains("orphan"));

    for path in [
        "$.\"khive:secret_gate\"",
        "$[\"khive:secret_gate\"]",
        "$.khive:secret_gate",
        "$",
    ] {
        let source = format!("fn write(store: &dyn NoteStore) {{ store.try_patch_note_property(id, ns, filter, {path:?}, value, now); }}");
        let sites = scan_sources(&[("sample/src/lib.rs".into(), source)]).unwrap();
        assert_eq!(sites[0].class, DetectedClass::WholeObject, "{path}");
        let row = RouteInventoryEntry {
            id: "synthetic.single-key",
            site: "sample/src/lib.rs::write",
            target: Substrate::Note,
            write_class: WriteClass::SingleKey { key_path: "$.safe" },
            reservation: Reservation::ByConstruction,
            ..ROUTE_INVENTORY[0]
        };
        assert!(
            check_inventory(&sites, &[row], 1)
                .unwrap_err()
                .contains("write class"),
            "{path}"
        );
    }

    let single_key_sql = [
        "UPDATE",
        "notes",
        "SET",
        "properties",
        "=",
        "json_set(properties,",
        "'$.read',",
        "1),",
        "updated_at",
        "=",
        "2",
        "WHERE",
        "json_extract(properties,",
        "'$.status')",
        "=",
        "'pending'",
    ]
    .join(" ");
    assert_eq!(sql_target(&single_key_sql), Some(Substrate::Note));
    assert_eq!(sql_single_key_path(&single_key_sql), Some("$.read".into()));
    for prefix in [
        vec!["INSERT", "INTO", "entities"],
        vec!["INSERT", "OR", "IGNORE", "INTO", "entities"],
        vec!["INSERT", "OR", "REPLACE", "INTO", "entities"],
        vec!["REPLACE", "INTO", "entities"],
    ] {
        assert_eq!(sql_target(&prefix.join(" ")), Some(Substrate::Entity));
    }
    for (replace, target) in [
        (
            "REPLACE INTO notes (id, properties) VALUES (?1, ?2)",
            Substrate::Note,
        ),
        (
            "REPLACE INTO entities (id, properties) VALUES (?1, ?2)",
            Substrate::Entity,
        ),
    ] {
        assert_eq!(sql_target(replace), Some(target));
        let replace_sites = scan_sources(&[(
            "sample/src/lib.rs".into(),
            format!("fn replace() {{ let _ = {replace:?}; }}"),
        )])
        .unwrap();
        assert_eq!(replace_sites[0].target, target);
        assert!(check_inventory(&replace_sites, &[], 0)
            .unwrap_err()
            .contains("unmapped sample/src/lib.rs::replace"));
    }
    assert_eq!(
        sql_target(&["UPDATE", "entities", "SET", "properties", "=", "?1"].join(" ")),
        Some(Substrate::Entity)
    );
    assert_eq!(
        sql_target(&["UPDATE", "notes", "SET", "updated_at", "=", "?1"].join(" ")),
        None
    );
    for sql in [
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_set(properties,",
            "'$.status',",
            "1,",
            "'$.at',",
            "2)",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_set(properties,",
            "'$.nested.key',",
            "1)",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_remove(json_set(properties,",
            "'$.safe',",
            "1),",
            "'$.other')",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
    ] {
        assert_eq!(sql_single_key_path(&sql), None, "{sql}");
    }

    let excluded = scan_sources(&[("sample/src/lib.rs".into(),
        "#[cfg(test)] mod tests { fn hidden(s: &dyn NoteStore, n: Note) { s.upsert_note(n); } } #[test] fn other(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into())]).unwrap();
    assert!(excluded.is_empty());
    assert!(check_inventory(&excluded, &[], 0).is_ok());

    let probe = (
        "sample/src/probe.rs".into(),
        "fn write(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into(),
    );
    for declaration in [
        "#[cfg(test)] #[path = \"probe.rs\"] mod tests;",
        "#[cfg(all(test, feature = \"extra\"))] #[path = \"probe.rs\"] mod tests;",
        "#[cfg(test)] mod probe;",
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), declaration.into()),
            probe.clone(),
        ];
        let sites = scan_sources(&sources).unwrap();
        assert!(sites.is_empty(), "{declaration}");
        assert!(check_inventory(&sites, &[], 0).is_ok(), "{declaration}");
    }
    for declaration in [
        "#[path = \"probe.rs\"] mod production;",
        "#[cfg(any(test, feature = \"extra\"))] #[path = \"probe.rs\"] mod production;",
        "#[cfg(test)] #[path = \"probe.rs\"] mod tests; #[path = \"probe.rs\"] mod production;",
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), declaration.into()),
            probe.clone(),
        ];
        let sites = scan_sources(&sources).unwrap();
        assert_eq!(sites.len(), 1, "{declaration}");
        assert!(
            check_inventory(&sites, &[], 0)
                .unwrap_err()
                .contains("unmapped"),
            "{declaration}"
        );
    }

    let transitive = scan_sources(&[
        (
            "sample/src/lib.rs".into(),
            "#[cfg(test)] #[path = \"probe.rs\"] mod tests;".into(),
        ),
        ("sample/src/probe.rs".into(), "mod nested;".into()),
        (
            "sample/src/probe/nested.rs".into(),
            "fn write(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into(),
        ),
    ])
    .unwrap();
    assert!(transitive.is_empty());

    let included_test_module = scan_sources(&[
        (
            "sample/src/lib.rs".into(),
            "include!(\"included.rs\");".into(),
        ),
        (
            "sample/src/included.rs".into(),
            "#[cfg(all(test, feature = \"extra\"))] mod tests { fn hidden(s: &dyn NoteStore, n: Note) { s.upsert_note(n); } }".into(),
        ),
    ])
    .unwrap();
    assert!(included_test_module.is_empty());

    let mut trait_sources = live_workspace_sources()
        .into_iter()
        .filter(|(path, _)| {
            path == "khive-storage/src/entity.rs" || path == "khive-storage/src/note.rs"
        })
        .collect::<Vec<_>>();
    assert!(check_store_trait_methods(&trait_sources).is_ok());
    let note_trait = trait_sources
        .iter_mut()
        .find(|(path, _)| path == "khive-storage/src/note.rs")
        .expect("note trait source");
    let declaration = "pub trait NoteStore: Send + Sync + 'static {";
    assert_eq!(note_trait.1.matches(declaration).count(), 1);
    note_trait.1 = note_trait.1.replacen(
        declaration,
        "pub trait NoteStore: Send + Sync + 'static { fn write_note_properties(&self) {}",
        1,
    );
    assert!(check_store_trait_methods(&trait_sources)
        .unwrap_err()
        .contains("unclassified store method write_note_properties"));
}
