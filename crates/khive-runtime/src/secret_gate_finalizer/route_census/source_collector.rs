use super::super::declaration::Substrate;
use super::ast_helpers::{literal_string, macro_strings, test_only};
use super::model::{
    DetectedClass, RouteClass, RuntimeTableSite, Site, SqlConstantReference, ENTITY_BUILDERS,
    NOTE_BUILDERS, SQL_CONSTANT_EVIDENCE, STORE_WRITES, UNCLASSIFIED_SQL,
};
use super::module_resolution::{
    qualified_target, shadow_bindings, use_bindings, Binding, ModuleBindings, ModuleId, SqlBindings,
};
use super::sql_routes::{
    bare_top_level_path, detected_key_class, detected_keys, entity_first,
    runtime_table_write_count, sql_fixed_key_paths, sql_write_occurrences,
};
use super::static_sql_source;
use super::static_sql_source::{CanonicalBindings, StaticSqlSources};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use syn::visit::Visit;
use syn::{
    Block, Expr, ExprCall, ExprLit, ExprMethodCall, FnArg, ImplItemFn, ItemConst, ItemEnum, ItemFn,
    ItemImpl, ItemMod, ItemStatic, ItemStruct, ItemTrait, ItemTraitAlias, ItemType, ItemUnion, Lit,
    Local, Macro, Stmt,
};

pub(super) struct SourceCollector<'modules> {
    pub(super) sql_sources: &'modules StaticSqlSources,
    pub(super) loader_bindings: CanonicalBindings,
    pub(super) sql_errors: Vec<String>,
    pub(super) resolved_sql: BTreeSet<String>,
    pub(super) path: String,
    pub(super) scope: Vec<String>,
    pub(super) sites: BTreeMap<String, Site>,
    pub(super) runtime_tables: BTreeMap<String, RuntimeTableSite>,
    pub(super) all_calls: BTreeMap<String, BTreeSet<String>>,
    pub(super) bindings: Vec<SqlBindings>,
    pub(super) parent_module_bindings: Vec<SqlBindings>,
    pub(super) module_id: ModuleId,
    pub(super) modules: &'modules ModuleBindings,
    pub(super) strict: bool,
    pub(super) path_ordinal: usize,
    pub(super) constant_references: BTreeMap<usize, SqlConstantReference>,
}

impl<'modules> SourceCollector<'modules> {
    pub(super) fn key(&self) -> String {
        format!("{}::{}", self.path, self.scope.join("::"))
    }

    pub(super) fn call(&mut self, function: &str) {
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

    pub(super) fn record(
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
    pub(super) fn scoped_binding(&self, first: &str) -> Option<Binding> {
        self.bindings
            .iter()
            .rev()
            .filter_map(|scope| scope.get(first))
            .find(|binding| **binding != Binding::Local)
            .cloned()
    }

    pub(super) fn record_sql(&mut self, literal: &str, observed: (usize, usize)) -> (usize, usize) {
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
pub(super) fn declared_binding(item: &syn::Item) -> Option<(String, Binding)> {
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
pub(super) fn with_declared_types(
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
