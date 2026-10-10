#![allow(dead_code)] // The two mounting crates use different halves of this shared oracle.
                     // FILESIZEJUSTIFICATION: one shared lexical/module resolver preserves the legacy oracle; production only supplies roots and observations.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;

pub(super) struct Keep {
    pub(super) name: &'static str,
    pub(super) path: &'static str,
    pub(super) sql: &'static str,
    pub(super) reason: &'static str,
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            part => result.push(part.as_os_str()),
        }
    }
    result
}

fn path_names(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

fn imports(tree: &syn::UseTree, prefix: &str, out: &mut Vec<(String, String)>) {
    match tree {
        syn::UseTree::Path(p) => imports(&p.tree, &format!("{prefix}{}::", p.ident), out),
        syn::UseTree::Name(n) => out.push((n.ident.to_string(), format!("{prefix}{}", n.ident))),
        syn::UseTree::Rename(n) => out.push((n.rename.to_string(), format!("{prefix}{}", n.ident))),
        syn::UseTree::Glob(_) => out.push(("*".into(), format!("{prefix}*"))),
        syn::UseTree::Group(g) => {
            for item in &g.items {
                imports(item, prefix, out);
            }
        }
    }
}

#[derive(Clone, Default)]
struct Scope {
    aliases: BTreeSet<String>,
    blocked: BTreeSet<String>,
    unknown_glob: bool,
    textual_unknown: bool,
}

impl Scope {
    fn unresolved(&self) -> bool {
        self.unknown_glob || self.textual_unknown
    }
}

struct Bindings {
    shared_macro: bool,
    runtime: bool,
    wrapper: bool,
    external_shadow: bool,
}

impl Bindings {
    fn canonical(&self, names: &[String], scope: &Scope, absolute: bool) -> bool {
        if !self.shared_macro {
            return false;
        }
        match names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["khive_runtime", "sql"] => {
                absolute
                    || (!self.external_shadow
                        && !scope.blocked.contains("khive_runtime")
                        && !scope.unresolved())
            }
            ["crate", "sql"] => self.runtime,
            ["crate", "sql", "sql"] => self.wrapper,
            [name] => {
                !scope.unresolved()
                    && scope.aliases.contains(*name)
                    && !scope.blocked.contains(*name)
            }
            _ => false,
        }
    }
}

/// Imports are item-scoped; conservatively reject any same-scope macro shadow,
/// including one declared later. No arbitrary macro expansion or glob guessing.
fn scope_for(
    items: &[&syn::Item],
    mut scope: Scope,
    parent: Option<&Scope>,
    bindings: &Bindings,
    child_exports: &dyn Fn(&syn::ItemMod) -> Option<BTreeSet<String>>,
) -> Scope {
    let mut uses = Vec::new();
    for item in items {
        if matches!(item, syn::Item::Mod(m) if m.attrs.iter().any(|a| a.path().is_ident("macro_use")))
            || matches!(item, syn::Item::ExternCrate(e) if e.attrs.iter().any(|a| a.path().is_ident("macro_use")))
        {
            scope.textual_unknown = true;
        }
        match item {
            syn::Item::Use(u) => imports(&u.tree, "", &mut uses),
            syn::Item::Mod(m) if m.ident == "khive_runtime" => {
                scope.blocked.insert(m.ident.to_string());
            }
            syn::Item::ExternCrate(e) => {
                let name = e.rename.as_ref().map_or(&e.ident, |(_, name)| name);
                if e.ident != "khive_runtime" || name != "khive_runtime" {
                    scope.blocked.insert(name.to_string());
                }
            }
            syn::Item::Macro(m) => {
                if let Some(name) = &m.ident {
                    scope.blocked.insert(name.to_string());
                }
            }
            _ => {}
        }
    }
    // Resolve root-name imports before checking canonical macro imports.
    for (name, path) in &uses {
        if name == "*" {
            if path == "super::*" {
                if let Some(parent) = parent {
                    scope.blocked.extend(parent.blocked.iter().cloned());
                    scope.aliases.extend(parent.aliases.iter().cloned());
                    scope.unknown_glob |= parent.unknown_glob;
                } else {
                    scope.unknown_glob = true;
                }
            } else if let Some(names) = local_glob(items, path).and_then(child_exports) {
                // A glob of a child module declared in this scope imports only that
                // module's names; each one may shadow, so each is blocked.
                scope.blocked.extend(names);
            } else {
                scope.unknown_glob = true;
            }
        } else if (name == "khive_runtime" && path != "khive_runtime") || name == "include_str" {
            scope.blocked.insert(name.clone());
        }
    }
    for (name, path) in uses {
        if name == "*" {
            continue;
        }
        let names = path.split("::").map(str::to_owned).collect::<Vec<_>>();
        if bindings.canonical(&names, &scope, false) {
            scope.aliases.insert(name);
        } else {
            scope.aliases.remove(&name);
            scope.blocked.insert(name);
        }
    }
    scope
}

/// The child module named by a `child::*` or `self::child::*` glob, when this scope
/// declares it.
fn local_glob<'a>(items: &[&'a syn::Item], path: &str) -> Option<&'a syn::ItemMod> {
    let name = path
        .strip_prefix("self::")
        .unwrap_or(path)
        .strip_suffix("::*")?;
    items.iter().copied().find_map(|item| match item {
        syn::Item::Mod(m) if m.ident == name => Some(m),
        _ => None,
    })
}

/// Every name a module declares, or None when one cannot be read from source: an
/// item macro may define items and a public glob re-export imports unknown names.
/// Private names are kept; blocking a name the glob cannot reach only withholds
/// credit. Private imports are skipped because a glob never re-exports them.
fn exports(items: &[syn::Item]) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for item in items {
        let ident = match item {
            syn::Item::Const(i) => &i.ident,
            syn::Item::Enum(i) => &i.ident,
            syn::Item::ExternCrate(i) => i.rename.as_ref().map_or(&i.ident, |(_, name)| name),
            syn::Item::Fn(i) => &i.sig.ident,
            syn::Item::Mod(i) => &i.ident,
            syn::Item::Static(i) => &i.ident,
            syn::Item::Struct(i) => &i.ident,
            syn::Item::Trait(i) => &i.ident,
            syn::Item::TraitAlias(i) => &i.ident,
            syn::Item::Type(i) => &i.ident,
            syn::Item::Union(i) => &i.ident,
            syn::Item::Use(u) => {
                if !matches!(u.vis, syn::Visibility::Inherited) {
                    let mut uses = Vec::new();
                    imports(&u.tree, "", &mut uses);
                    for (name, _) in uses {
                        if name == "*" {
                            return None;
                        }
                        names.insert(name);
                    }
                }
                continue;
            }
            syn::Item::Impl(_) => continue,
            // macro_rules! scope is textual; a glob does not import it.
            syn::Item::Macro(m) if m.ident.is_some() => continue,
            _ => return None,
        };
        names.insert(ident.to_string());
    }
    Some(names)
}

fn static_string(
    expr: &syn::Expr,
    crate_root: &Path,
    scope: &Scope,
) -> Result<String, &'static str> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Ok(s.value()),
        syn::Expr::Macro(m)
            if m.mac.path.is_ident("concat") && !scope.blocked.contains("concat") =>
        {
            let args = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(m.mac.tokens.clone())
                .map_err(|_| "invalid concat arguments")?;
            args.iter()
                .map(|e| static_string(e, crate_root, scope))
                .collect::<Result<Vec<_>, _>>()
                .map(|s| s.concat())
        }
        syn::Expr::Macro(m) if m.mac.path.is_ident("env") && !scope.blocked.contains("env") => {
            let name = syn::parse2::<syn::LitStr>(m.mac.tokens.clone())
                .map_err(|_| "invalid env argument")?;
            if name.value() == "CARGO_MANIFEST_DIR" {
                Ok(crate_root.to_string_lossy().into_owned())
            } else {
                Err("only CARGO_MANIFEST_DIR is static here")
            }
        }
        _ => Err("SQL include requires a literal or static concat/env expression"),
    }
}

#[derive(Clone, Debug)]
pub(super) struct Site {
    pub path: PathBuf,
    pub range: std::ops::Range<usize>,
    pub value: String,
    pub kind: &'static str,
    pub mounts: Vec<(PathBuf, std::ops::Range<usize>)>,
}
#[derive(Default)]
pub(super) struct Observations {
    pub readers: Vec<Site>,
    pub literals: Vec<Site>,
    pub diagnostics: Vec<String>,
    pub visited: BTreeSet<PathBuf>,
    pub requested: BTreeSet<PathBuf>,
}
struct References<'a> {
    path: PathBuf,
    module_dir: PathBuf,
    inline: bool,
    parsed: &'a BTreeMap<PathBuf, syn::File>,
    visited: &'a mut BTreeSet<PathBuf>,
    active: BTreeSet<PathBuf>,
    parent: Option<Scope>,
    crate_root: &'a Path,
    bindings: &'a Bindings,
    scope: Scope,
    used: &'a mut BTreeSet<PathBuf>,
    errors: &'a mut Vec<String>,
    observed: Option<&'a mut Observations>,
    path_overrides: &'a BTreeMap<(PathBuf, usize), String>,
    mounts: Vec<(PathBuf, std::ops::Range<usize>)>,
    excluded: &'a BTreeMap<PathBuf, Vec<std::ops::Range<usize>>>,
    verified_macro: bool,
    literal_kind: &'static str,
}

impl References<'_> {
    fn admitted_at(&self, path: &Path, span: proc_macro2::Span) -> bool {
        let range = span.byte_range();
        self.observed.is_none()
            || !self.excluded.get(path).is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|r| r.start <= range.start && r.end >= range.end)
            })
    }
    fn admitted(&self, span: proc_macro2::Span) -> bool {
        self.admitted_at(&self.path, span)
    }
    fn record(&mut self, path: PathBuf) {
        // A cross-crate include is valid Rust but does not establish own-crate use.
        let path = normalize(&path);
        if path.starts_with(self.crate_root) {
            self.used.insert(path);
        }
    }
    fn site(&self, span: proc_macro2::Span, value: String, kind: &'static str) -> Site {
        Site {
            path: self.path.clone(),
            range: span.byte_range(),
            value,
            kind,
            mounts: self.mounts.clone(),
        }
    }
    fn reader(&mut self, span: proc_macro2::Span, path: PathBuf, kind: &'static str) {
        let normalized = normalize(&path);
        self.record(path);
        let site = self.site(span, normalized.to_string_lossy().into_owned(), kind);
        if let Some(observed) = self.observed.as_mut() {
            observed.readers.push(site);
        }
    }
    fn literal(&mut self, span: proc_macro2::Span, value: String) {
        let site = self.site(span, value, self.literal_kind);
        if let Some(observed) = self.observed.as_mut() {
            observed.literals.push(site);
        }
    }
    fn error(&mut self, message: &str) {
        self.errors
            .push(format!("{}: {message}", self.path.display()));
    }
    /// The names a glob of this child module imports, read from its own source.
    fn child_exports(&self, module: &syn::ItemMod) -> Option<BTreeSet<String>> {
        if module.attrs.iter().any(|a| a.path().is_ident("path")) {
            return None;
        }
        match &module.content {
            Some((_, items)) => exports(
                &items
                    .iter()
                    .filter(|i| self.admitted(i.span()))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            None => {
                let base = self.module_dir.join(module.ident.to_string());
                [base.with_extension("rs"), base.join("mod.rs")]
                    .iter()
                    .find_map(|path| self.parsed.get(path).map(|file| (path, file)))
                    .and_then(|(path, file)| {
                        exports(
                            &file
                                .items
                                .iter()
                                .filter(|i| self.admitted_at(path, i.span()))
                                .cloned()
                                .collect::<Vec<_>>(),
                        )
                    })
            }
        }
    }
}

impl<'ast> Visit<'ast> for References<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if !self.admitted(file.span()) {
            return;
        }
        self.visited.insert(self.path.clone());
        if let Some(observed) = self.observed.as_mut() {
            observed.visited.insert(self.path.clone());
        }
        self.scope = scope_for(
            &file
                .items
                .iter()
                .filter(|i| self.admitted(i.span()))
                .collect::<Vec<_>>(),
            self.scope.clone(),
            self.parent.as_ref(),
            self.bindings,
            &|module: &syn::ItemMod| self.child_exports(module),
        );
        syn::visit::visit_file(self, file);
    }
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let old = self.scope.clone();
        let old_dir = self.module_dir.clone();
        let old_inline = self.inline;
        let inherited = Scope {
            blocked: old.blocked.clone(),
            textual_unknown: old.textual_unknown,
            ..Scope::default()
        };
        let explicit = module
            .attrs
            .iter()
            .find(|a| a.path().is_ident("path"))
            .and_then(|a| {
                if let syn::Meta::NameValue(n) = &a.meta {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = &n.value
                    {
                        return Some(s.value());
                    }
                }
                None
            });
        let explicit = self
            .path_overrides
            .get(&(self.path.clone(), module.span().byte_range().start))
            .cloned()
            .or(explicit);
        let old_mounts = self.mounts.clone();
        self.mounts
            .push((self.path.clone(), module.span().byte_range()));
        // #[path] at file scope is source-directory relative. Within inline
        // modules it is relative to their directory, including a non-mod-rs stem.
        let path_base = if self.inline {
            old_dir.clone()
        } else {
            self.path.parent().unwrap().to_path_buf()
        };
        if let Some((_, items)) = &module.content {
            self.module_dir = explicit.map_or_else(
                || old_dir.join(module.ident.to_string()),
                |p| normalize(&path_base.join(p)),
            );
            self.inline = true;
            self.scope = scope_for(
                &items
                    .iter()
                    .filter(|i| self.admitted(i.span()))
                    .collect::<Vec<_>>(),
                inherited.clone(),
                Some(&old),
                self.bindings,
                &|module: &syn::ItemMod| self.child_exports(module),
            );
            for item in items {
                self.visit_item(item);
            }
        } else {
            let default = old_dir.join(module.ident.to_string());
            let candidates = explicit.map_or_else(
                || vec![default.with_extension("rs"), default.join("mod.rs")],
                |p| vec![normalize(&path_base.join(p))],
            );
            let parsed = self.parsed;
            if let Some(observed) = self.observed.as_mut() {
                observed.requested.extend(candidates.iter().cloned());
                let available = candidates
                    .iter()
                    .filter(|p| parsed.contains_key(*p))
                    .count();
                if available != 1 {
                    self.error("missing, malformed, escaping or ambiguous production module");
                }
            }
            for path in candidates {
                if let Some(file) = parsed.get(&path) {
                    if !self.active.insert(path.clone()) {
                        self.error("recursive source module");
                        continue;
                    }
                    let old_path = std::mem::replace(&mut self.path, path.clone());
                    let old_parent = self.parent.replace(old.clone());
                    self.scope = inherited.clone();
                    self.module_dir = module_directory(&path);
                    self.inline = false;
                    <Self as Visit<'_>>::visit_file(self, file);
                    self.parent = old_parent;
                    self.path = old_path;
                    self.active.remove(&path);
                }
            }
        }
        self.scope = old;
        self.module_dir = old_dir;
        self.inline = old_inline;
        self.mounts = old_mounts;
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let old = self.scope.clone();
        let items = block
            .stmts
            .iter()
            .filter_map(|s| {
                if let syn::Stmt::Item(i) = s {
                    self.admitted(i.span()).then_some(i)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.scope = scope_for(&items, old.clone(), Some(&old), self.bindings, &|_| None);
        syn::visit::visit_block(self, block);
        self.scope = old;
    }
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if self.observed.is_none() {
            syn::visit::visit_attribute(self, attr);
        }
    }
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if self.admitted(item.span()) {
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        if self.admitted(expr.span()) {
            syn::visit::visit_expr(self, expr);
        }
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if self.admitted(local.span()) {
            syn::visit::visit_local(self, local);
        }
    }
    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if self.admitted(node.span()) {
            syn::visit::visit_stmt_macro(self, node);
        }
    }
    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if self.admitted(node.span()) {
            syn::visit::visit_arm(self, node);
        }
    }
    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if self.admitted(node.span()) {
            syn::visit::visit_field_value(self, node);
        }
    }
    fn visit_field(&mut self, node: &'ast syn::Field) {
        if self.admitted(node.span()) {
            syn::visit::visit_field(self, node);
        }
    }
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if self.admitted(node.span()) {
            syn::visit::visit_variant(self, node);
        }
    }
    fn visit_pat_type(&mut self, node: &'ast syn::PatType) {
        if self.admitted(node.span()) {
            syn::visit::visit_pat_type(self, node);
        }
    }
    fn visit_const_param(&mut self, node: &'ast syn::ConstParam) {
        if self.admitted(node.span()) {
            syn::visit::visit_const_param(self, node);
        }
    }
    fn visit_type_param(&mut self, node: &'ast syn::TypeParam) {
        if self.admitted(node.span()) {
            syn::visit::visit_type_param(self, node);
        }
    }
    fn visit_lifetime_param(&mut self, node: &'ast syn::LifetimeParam) {
        if self.admitted(node.span()) {
            syn::visit::visit_lifetime_param(self, node);
        }
    }
    fn visit_bare_fn_arg(&mut self, node: &'ast syn::BareFnArg) {
        if self.admitted(node.span()) {
            syn::visit::visit_bare_fn_arg(self, node);
        }
    }
    fn visit_bare_variadic(&mut self, node: &'ast syn::BareVariadic) {
        if self.admitted(node.span()) {
            syn::visit::visit_bare_variadic(self, node);
        }
    }
    fn visit_receiver(&mut self, node: &'ast syn::Receiver) {
        if self.admitted(node.span()) {
            syn::visit::visit_receiver(self, node);
        }
    }
    fn visit_variadic(&mut self, node: &'ast syn::Variadic) {
        if self.admitted(node.span()) {
            syn::visit::visit_variadic(self, node);
        }
    }
    fn visit_field_pat(&mut self, node: &'ast syn::FieldPat) {
        if self.admitted(node.span()) {
            syn::visit::visit_field_pat(self, node);
        }
    }
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        if self.admitted(node.span()) {
            syn::visit::visit_pat(self, node);
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if self.admitted(item.span()) {
            syn::visit::visit_impl_item(self, item);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if self.admitted(item.span()) {
            syn::visit::visit_trait_item(self, item);
        }
    }
    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        if self.admitted(item.span()) {
            syn::visit::visit_foreign_item(self, item);
        }
    }
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        // Declarations contain symbolic tokens, not an invocation of their expansion.
        if item.ident.is_none() {
            self.visit_macro(&item.mac);
        }
    }
    fn visit_expr_lit(&mut self, expr: &'ast syn::ExprLit) {
        if let syn::Lit::Str(value) = &expr.lit {
            self.literal(expr.span(), value.value());
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let names = path_names(&mac.path);
        if self
            .bindings
            .canonical(&names, &self.scope, mac.path.leading_colon.is_some())
        {
            if self.observed.is_some() && !self.verified_macro {
                self.error("canonical SQL macro definition or mounting is not verified");
                return;
            }
            match syn::parse2::<syn::LitStr>(mac.tokens.clone()) {
                Ok(name)
                    if !name.value().is_empty()
                        && name
                            .value()
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
                {
                    self.reader(
                        mac.span(),
                        self.crate_root
                            .join("sql")
                            .join(format!("{}.sql", name.value())),
                        "sql!",
                    );
                }
                _ => self.error("sql! requires one literal statement name"),
            }
        } else if self.observed.is_some() && mac.path.is_ident("sql") && self.scope.unresolved() {
            self.error("SQL loader has unresolved lexical macro bindings");
        } else if mac.path.is_ident("include_str") && !self.scope.blocked.contains("include_str") {
            let result = syn::parse2::<syn::Expr>(mac.tokens.clone())
                .map_err(|_| "invalid include_str argument")
                .and_then(|expr| static_string(&expr, self.crate_root, &self.scope));
            match result {
                Ok(name) if Path::new(&name).extension().is_some_and(|e| e == "sql") => {
                    if self.scope.unresolved() {
                        self.error("SQL include has unresolved glob macro bindings");
                    } else {
                        self.reader(
                            mac.span(),
                            self.path.parent().unwrap().join(name),
                            "include_str!",
                        );
                    }
                }
                Ok(_) => {} // JSON, Rust and documentation includes are not SQL assets.
                Err(error) => self.error(error),
            }
        } else if self.observed.is_some()
            && mac.path.is_ident("concat")
            && !self.scope.blocked.contains("concat")
            && !self.scope.unresolved()
        {
            let expr = syn::Expr::Macro(syn::ExprMacro {
                attrs: Vec::new(),
                mac: mac.clone(),
            });
            match static_string(&expr, self.crate_root, &self.scope) {
                Ok(value) => self.literal(mac.span(), value),
                Err(error) => self.error(error),
            }
        } else if [
            "assert",
            "assert_eq",
            "assert_ne",
            "debug_assert",
            "debug_assert_eq",
            "debug_assert_ne",
            "format",
            "vec",
        ]
        .iter()
        .any(|name| {
            mac.path.is_ident(name)
                && !self.scope.blocked.contains(*name)
                && !self.scope.unresolved()
        }) || (self.observed.is_some()
            && [
                "println", "eprintln", "print", "eprint", "panic", "write", "writeln",
            ]
            .iter()
            .any(|name| {
                mac.path.is_ident(name)
                    && !self.scope.blocked.contains(*name)
                    && !self.scope.unresolved()
            }))
        {
            // Known expression macros may contain loaders; arbitrary token-producing
            // macros such as quote! confer no asset credit.
            if let Ok(args) =
                syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                    .parse2(mac.tokens.clone())
            {
                for (index, expr) in args.iter().enumerate() {
                    let old_kind = self.literal_kind;
                    let template = (["format", "println", "eprintln", "print", "eprint", "panic"]
                        .iter()
                        .any(|name| mac.path.is_ident(name))
                        && index == 0)
                        || (["write", "writeln"]
                            .iter()
                            .any(|name| mac.path.is_ident(name))
                            && index == 1);
                    if self.observed.is_some()
                        && template
                        && matches!(
                            expr,
                            syn::Expr::Lit(syn::ExprLit {
                                lit: syn::Lit::Str(_),
                                ..
                            }) | syn::Expr::Macro(_)
                        )
                    {
                        self.literal_kind = "format-template";
                    }
                    <Self as Visit<'_>>::visit_expr(self, expr);
                    self.literal_kind = old_kind;
                }
            } else if self.observed.is_some() && sql_looking(&mac.tokens) {
                self.error("unsupported SQL-looking expression macro arguments");
            }
        } else if self.observed.is_some() {
            if sql_looking(&mac.tokens) {
                self.error("SQL-looking opaque or unresolved macro input");
            }
        }
    }
}

fn module_directory(path: &Path) -> PathBuf {
    if ["lib.rs", "main.rs", "mod.rs"]
        .iter()
        .any(|n| path.file_name().is_some_and(|f| f == *n))
    {
        path.parent().unwrap().to_path_buf()
    } else {
        path.with_extension("")
    }
}

pub(super) fn reference_errors(
    sources: &BTreeMap<PathBuf, String>,
    assets: &BTreeSet<PathBuf>,
    root: &Path,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut used = BTreeSet::new();
    let shared_macro = shared_identity(sources, root, None);
    let crates = assets
        .iter()
        .map(|p| p.parent().unwrap().parent().unwrap().to_path_buf())
        .collect::<BTreeSet<_>>();
    for crate_root in crates {
        let roots = ["src/lib.rs", "src/main.rs"]
            .iter()
            .filter_map(|p| sources.get(&crate_root.join(p)))
            .filter_map(|s| syn::parse_file(s).ok())
            .collect::<Vec<_>>();
        let root_items = roots.iter().flat_map(|f| &f.items).collect::<Vec<_>>();
        let external_shadow = root_items
            .iter()
            .any(|i| matches!(i, syn::Item::Mod(m) if m.ident == "khive_runtime"));
        let mut bindings = Bindings {
            shared_macro,
            runtime: crate_root == root.join("khive-runtime"),
            wrapper: false,
            external_shadow,
        };
        let wrapper_declared = root_items.iter().any(|i| {
            matches!(i, syn::Item::Mod(m) if m.ident == "sql" && m.content.is_none()
            && !m.attrs.iter().any(|a| a.path().is_ident("path")))
        });
        bindings.wrapper = wrapper_declared
            && sources
                .get(&crate_root.join("src/sql.rs"))
                .and_then(|s| syn::parse_file(s).ok())
                .is_some_and(|f| {
                    let scope = scope_for(
                        &f.items.iter().collect::<Vec<_>>(),
                        Scope::default(),
                        None,
                        &bindings,
                        &|_| None,
                    );
                    scope.aliases.contains("sql") && !scope.blocked.contains("sql")
                });
        let mut parsed = BTreeMap::new();
        for (path, text) in sources.iter().filter(|(p, _)| p.starts_with(&crate_root)) {
            match syn::parse_file(text) {
                Ok(file) => {
                    parsed.insert(path.clone(), file);
                }
                Err(e) => errors.push(format!("parse {}: {e}", path.display())),
            }
        }
        if parsed.is_empty() {
            errors.push(format!("no source inputs for {}", crate_root.display()));
        }
        let mut visited = BTreeSet::new();
        // Resolve real module ancestry first; remaining source/test files still belong
        // to the unchanged inventory, even when not linked on the host platform.
        let paths = ["src/lib.rs", "src/main.rs"]
            .iter()
            .map(|p| crate_root.join(p))
            .chain(parsed.keys().cloned())
            .collect::<Vec<_>>();
        for path in paths {
            if visited.contains(&path) {
                continue;
            }
            if let Some(file) = parsed.get(&path) {
                References {
                    module_dir: module_directory(&path),
                    inline: false,
                    active: BTreeSet::from([path.clone()]),
                    path,
                    parsed: &parsed,
                    visited: &mut visited,
                    parent: None,
                    crate_root: &crate_root,
                    bindings: &bindings,
                    scope: Scope::default(),
                    used: &mut used,
                    errors: &mut errors,
                    observed: None,
                    path_overrides: &BTreeMap::new(),
                    mounts: Vec::new(),
                    excluded: &BTreeMap::new(),
                    verified_macro: true,
                    literal_kind: "literal",
                }
                .visit_file(file);
            }
        }
    }
    for asset in assets.difference(&used) {
        errors.push(format!("unused SQL: {}", asset.display()));
    }
    for missing in used.difference(assets) {
        // The inventory is each crate's sql/*.sql. An SQL file a crate includes from
        // elsewhere, such as a documented query used as a test oracle, has to exist
        // but is not inventoried.
        let inventoried = missing.strip_prefix(root).map_or(true, |path| {
            path.components()
                .nth(1)
                .is_some_and(|c| c.as_os_str() == "sql")
        });
        if inventoried || !missing.is_file() {
            errors.push(format!("missing SQL: {}", missing.display()));
        }
    }
    errors
}

fn production_source(path: &Path) -> bool {
    let path_text = path.to_string_lossy();
    !path_text.contains("/tests/")
        && !path_text.contains("/benches/")
        && path
            .file_name()
            .is_some_and(|n| n != "tests.rs" && !n.to_string_lossy().ends_with("_tests.rs"))
}

pub(super) fn still_inline_seen(sources: &BTreeMap<PathBuf, String>, crate_root: &Path) -> bool {
    sources.iter().any(|(p, text)| {
        p.starts_with(crate_root)
            && production_source(p)
            && !sql_literals(&strip_test_modules(text)).is_empty()
    })
}

pub(super) fn inline_errors(
    sources: &BTreeMap<PathBuf, String>,
    root: &Path,
    converted: &[&str],
    keeps: &[Keep],
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut matches = vec![0usize; keeps.len()];
    let mut names = BTreeSet::new();
    let mut statements = BTreeSet::new();
    for keep in keeps {
        if keep.name.is_empty()
            || keep.reason.trim().is_empty()
            || !names.insert(keep.name)
            || !statements.insert((keep.path, keep.sql))
        {
            errors.push(format!("invalid or duplicate inline keep: {}", keep.name));
        }
    }
    for (path, text) in sources {
        let relative = path.strip_prefix(root).unwrap().to_string_lossy();
        let crate_name = relative.split('/').next().unwrap();
        // A named keep can opt one source file into the policy before its
        // entire crate is converted. Other unconverted files stay excluded.
        let explicitly_kept = keeps.iter().any(|keep| keep.path == relative);
        if (!converted.contains(&crate_name) && !explicitly_kept) || !production_source(path) {
            continue;
        }
        for statement in sql_literals(&strip_test_modules(text)) {
            if let Some((i, _)) = keeps
                .iter()
                .enumerate()
                .find(|(_, k)| k.path == relative && k.sql == statement)
            {
                matches[i] += 1;
            } else {
                errors.push(format!("inline SQL: {relative}: {statement}"));
            }
        }
    }
    for (keep, count) in keeps.iter().zip(matches) {
        if count != 1 {
            errors.push(format!(
                "inline keep {} expected once, found {count}",
                keep.name
            ));
        }
    }
    errors
}

pub(super) fn strip_test_modules(text: &str) -> String {
    struct TestModules(Vec<std::ops::Range<usize>>);

    impl<'ast> Visit<'ast> for TestModules {
        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            let test_only = module.attrs.iter().any(|attr| {
                attr.path().is_ident("cfg")
                    && attr
                        .parse_args::<syn::Path>()
                        .is_ok_and(|path| path.is_ident("test"))
            });
            if module.content.is_some() && test_only {
                self.0.push(module.span().byte_range());
                return;
            }
            syn::visit::visit_item_mod(self, module);
        }
    }

    let file = syn::parse_file(text).expect("parse Rust source for inline SQL policy");
    // syn strips these prefixes before parsing; its spans start after them.
    let offset = if text.starts_with('\u{feff}') { 3 } else { 0 }
        + file.shebang.as_ref().map_or(0, String::len);
    let mut modules = TestModules(Vec::new());
    modules.visit_file(&file);

    // Keep every byte outside an excluded item and every source line boundary.
    // Blanking complete items also excludes their test-only outer attributes.
    let mut out = text.as_bytes().to_vec();
    for range in modules.0 {
        for byte in &mut out[offset + range.start..offset + range.end] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(out).expect("masked Rust source remains UTF-8")
}

/// Find a literal's closing quote, skipping rather than decoding escapes.
fn literal_body(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return text.get(open + 1..i),
            _ => i += 1,
        }
    }
    None
}

/// Flatten real newlines, escaped whitespace and line continuations alike.
/// Dropping only the backslash in `\nFROM` would hide the structural keyword.
fn flatten(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.clone().next() {
            // An escape that stands for whitespace: consume both characters.
            Some('n' | 't' | 'r') => {
                chars.next();
                out.push(' ');
            }
            // A line continuation, or any other escape: the backslash goes,
            // what follows is kept and judged on its own.
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn sql_literals(text: &str) -> Vec<String> {
    // Preserve the existing case-sensitive verb + structural-keyword predicate.
    // A leading verb alone also matches English descriptions and error labels.
    const SHAPES: [(&str, &[&str]); 10] = [
        // CTEs start with WITH rather than a statement verb.
        (
            "WITH ",
            &[" AS (", " AS MATERIALIZED (", " AS NOT MATERIALIZED ("],
        ),
        ("SELECT ", &[" FROM "]),
        ("INSERT ", &["INSERT INTO ", "INSERT OR "]),
        ("UPDATE ", &[" SET "]),
        ("DELETE ", &["DELETE FROM "]),
        (
            "CREATE ",
            &[
                "CREATE TABLE",
                "CREATE INDEX",
                "CREATE UNIQUE",
                "CREATE VIEW",
                "CREATE VIRTUAL",
                "CREATE TRIGGER",
            ],
        ),
        (
            "DROP ",
            &["DROP TABLE", "DROP INDEX", "DROP VIEW", "DROP TRIGGER"],
        ),
        ("ALTER ", &["ALTER TABLE"]),
        ("PRAGMA ", &["PRAGMA "]),
        ("REPLACE ", &["REPLACE INTO "]),
    ];
    let mut found = Vec::new();
    for (index, _) in text.match_indices('"') {
        let Some(body) = literal_body(text, index) else {
            continue;
        };
        let flat = flatten(body);
        let Some((_, seconds)) = SHAPES.iter().find(|(v, _)| flat.starts_with(*v)) else {
            continue;
        };
        if !seconds.iter().any(|second| flat.contains(second)) {
            continue;
        }
        found.push(flat);
    }
    found
}

fn shared_identity(
    sources: &BTreeMap<PathBuf, String>,
    root: &Path,
    excluded: Option<&BTreeMap<PathBuf, Vec<std::ops::Range<usize>>>>,
) -> bool {
    let admitted = |path: &Path, span: proc_macro2::Span| {
        let range = span.byte_range();
        !excluded.is_some_and(|map| {
            map.get(path).is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|r| r.start <= range.start && r.end >= range.end)
            })
        })
    };
    let shared_path = root.join("khive-runtime/src/sql_include.rs");
    let expected: proc_macro2::TokenStream = r#"($name:literal) => { const {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql")).trim_ascii_end()
    } };"#
        .parse()
        .unwrap();
    let shared_macro = sources
        .get(&shared_path)
        .and_then(|s| syn::parse_file(s).ok())
        .is_some_and(|f| {
            admitted(&shared_path, f.span()) && f.items.iter().any(|i| {
                admitted(&shared_path, i.span()) && matches!(i, syn::Item::Macro(m) if m.ident.as_ref().is_some_and(|n| n == "sql")
            && m.attrs.iter().any(|a| a.path().is_ident("macro_export"))
            && m.mac.tokens.to_string() == expected.to_string())
            })
        });
    let shared_macro = shared_macro
        && sources
            .get(&root.join("khive-runtime/src/lib.rs"))
            .and_then(|s| syn::parse_file(s).ok())
            .is_some_and(|f| {
                admitted(&root.join("khive-runtime/src/lib.rs"), f.span()) && f.items.iter().any(|i| {
                    admitted(&root.join("khive-runtime/src/lib.rs"), i.span()) && matches!(i, syn::Item::Mod(m) if m.ident == "sql_include" && m.content.is_none()
                && !m.attrs.iter().any(|a| a.path().is_ident("path")))
                })
            });
    shared_macro
}

/// Production supplies masked immutable source and explicit Cargo roots; no fallback.
pub(super) fn observe(
    sources: &BTreeMap<PathBuf, String>,
    parsed: &BTreeMap<PathBuf, syn::File>,
    root: &Path,
    crate_root: &Path,
    entry: &Path,
    path_overrides: &BTreeMap<(PathBuf, usize), String>,
    excluded: &BTreeMap<PathBuf, Vec<std::ops::Range<usize>>>,
) -> Observations {
    let mut observed = Observations::default();
    let shared_macro = shared_identity(sources, root, Some(excluded));
    let Some(file) = parsed.get(entry) else {
        observed.diagnostics.push(format!(
            "missing or malformed production root {}",
            entry.display()
        ));
        return observed;
    };
    let allowed = |item: &&syn::Item| {
        let range = item.span().byte_range();
        !excluded.get(entry).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|r| r.start <= range.start && r.end >= range.end)
        })
    };
    let root_items = file.items.iter().filter(allowed).collect::<Vec<_>>();
    let mut bindings = Bindings {
        shared_macro: true,
        runtime: crate_root == root.join("khive-runtime"),
        wrapper: false,
        external_shadow: root_items
            .iter()
            .any(|i| matches!(i, syn::Item::Mod(m) if m.ident == "khive_runtime")),
    };
    bindings.wrapper = root_items.iter().any(|i| {
        matches!(i, syn::Item::Mod(m) if m.ident == "sql" && m.content.is_none()
        && !m.attrs.iter().any(|a| a.path().is_ident("path")))
    }) && parsed.get(&crate_root.join("src/sql.rs")).is_some_and(|f| {
        let scope = scope_for(
            &f.items
                .iter()
                .filter(|item| {
                    let range = item.span().byte_range();
                    !excluded
                        .get(&crate_root.join("src/sql.rs"))
                        .is_some_and(|ranges| {
                            ranges
                                .iter()
                                .any(|r| r.start <= range.start && r.end >= range.end)
                        })
                })
                .collect::<Vec<_>>(),
            Scope::default(),
            None,
            &bindings,
            &|_| None,
        );
        scope.aliases.contains("sql") && !scope.blocked.contains("sql")
    });
    let mut visited = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut errors = Vec::new();
    References {
        path: entry.to_path_buf(),
        module_dir: module_directory(entry),
        inline: false,
        parsed,
        visited: &mut visited,
        active: BTreeSet::from([entry.to_path_buf()]),
        parent: None,
        crate_root,
        bindings: &bindings,
        scope: Scope::default(),
        used: &mut used,
        errors: &mut errors,
        observed: Some(&mut observed),
        path_overrides,
        mounts: Vec::new(),
        excluded,
        verified_macro: shared_macro,
        literal_kind: "literal",
    }
    .visit_file(file);
    observed.diagnostics.extend(errors);
    observed
}

fn sql_looking(tokens: &proc_macro2::TokenStream) -> bool {
    tokens.clone().into_iter().any(|token| match token {
        proc_macro2::TokenTree::Group(group) => sql_looking(&group.stream()),
        proc_macro2::TokenTree::Literal(value) => syn::parse_str::<syn::LitStr>(&value.to_string())
            .is_ok_and(|s| {
                let value = s.value();
                let first = value.trim_start().split_whitespace().next().unwrap_or("");
                [
                    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "ALTER", "DROP", "WITH",
                    "REPLACE", "PRAGMA",
                ]
                .contains(&first)
            }),
        _ => false,
    })
}
