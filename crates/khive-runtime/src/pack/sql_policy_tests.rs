use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;

const CONVERTED: &[&str] = &[
    "khive-pack-brain",
    "khive-pack-comm",
    "khive-pack-git",
    "khive-pack-gtd",
    "khive-pack-kg",
    "khive-pack-knowledge",
    "khive-pack-memory",
    "khive-retrieval",
    "kkernel",
];

/// The SQL spelling predicate deliberately does not parse arbitrary assembled SQL.
struct Keep {
    name: &'static str,
    path: &'static str,
    sql: &'static str,
    reason: &'static str,
}

fn normalize(path: &Path) -> PathBuf {
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
}

impl References<'_> {
    fn record(&mut self, path: PathBuf) {
        // A cross-crate include is valid Rust but does not establish own-crate use.
        let path = normalize(&path);
        if path.starts_with(self.crate_root) {
            self.used.insert(path);
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
            Some((_, items)) => exports(items),
            None => {
                let base = self.module_dir.join(module.ident.to_string());
                [base.with_extension("rs"), base.join("mod.rs")]
                    .iter()
                    .find_map(|path| self.parsed.get(path))
                    .and_then(|file| exports(&file.items))
            }
        }
    }
}

impl<'ast> Visit<'ast> for References<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.visited.insert(self.path.clone());
        self.scope = scope_for(
            &file.items.iter().collect::<Vec<_>>(),
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
                &items.iter().collect::<Vec<_>>(),
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
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let old = self.scope.clone();
        let items = block
            .stmts
            .iter()
            .filter_map(|s| {
                if let syn::Stmt::Item(i) = s {
                    Some(i)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.scope = scope_for(&items, old.clone(), Some(&old), self.bindings, &|_| None);
        syn::visit::visit_block(self, block);
        self.scope = old;
    }
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        // Declarations contain symbolic tokens, not an invocation of their expansion.
        if item.ident.is_none() {
            self.visit_macro(&item.mac);
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let names = path_names(&mac.path);
        if self
            .bindings
            .canonical(&names, &self.scope, mac.path.leading_colon.is_some())
        {
            match syn::parse2::<syn::LitStr>(mac.tokens.clone()) {
                Ok(name)
                    if !name.value().is_empty()
                        && name
                            .value()
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
                {
                    self.record(
                        self.crate_root
                            .join("sql")
                            .join(format!("{}.sql", name.value())),
                    );
                }
                _ => self.error("sql! requires one literal statement name"),
            }
        } else if mac.path.is_ident("include_str") && !self.scope.blocked.contains("include_str") {
            let result = syn::parse2::<syn::Expr>(mac.tokens.clone())
                .map_err(|_| "invalid include_str argument")
                .and_then(|expr| static_string(&expr, self.crate_root, &self.scope));
            match result {
                Ok(name) if Path::new(&name).extension().is_some_and(|e| e == "sql") => {
                    if self.scope.unresolved() {
                        self.error("SQL include has unresolved glob macro bindings");
                    } else {
                        self.record(self.path.parent().unwrap().join(name));
                    }
                }
                Ok(_) => {} // JSON, Rust and documentation includes are not SQL assets.
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
        }) {
            // Known expression macros may contain loaders; arbitrary token-producing
            // macros such as quote! confer no asset credit.
            if let Ok(args) =
                syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                    .parse2(mac.tokens.clone())
            {
                for expr in &args {
                    <Self as Visit<'_>>::visit_expr(self, expr);
                }
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

fn reference_errors(
    sources: &BTreeMap<PathBuf, String>,
    assets: &BTreeSet<PathBuf>,
    root: &Path,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut used = BTreeSet::new();
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
            f.items.iter().any(|i| {
                matches!(i, syn::Item::Macro(m) if m.ident.as_ref().is_some_and(|n| n == "sql")
            && m.attrs.iter().any(|a| a.path().is_ident("macro_export"))
            && m.mac.tokens.to_string() == expected.to_string())
            })
        });
    let shared_macro = shared_macro
        && sources
            .get(&root.join("khive-runtime/src/lib.rs"))
            .and_then(|s| syn::parse_file(s).ok())
            .is_some_and(|f| {
                f.items.iter().any(|i| {
                    matches!(i, syn::Item::Mod(m) if m.ident == "sql_include" && m.content.is_none()
                && !m.attrs.iter().any(|a| a.path().is_ident("path")))
                })
            });
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

fn still_inline_seen(sources: &BTreeMap<PathBuf, String>, crate_root: &Path) -> bool {
    sources.iter().any(|(p, text)| {
        p.starts_with(crate_root)
            && production_source(p)
            && !sql_literals(&strip_test_modules(text)).is_empty()
    })
}

fn inline_errors(
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
        if !converted.contains(&crate_name) || !production_source(path) {
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

pub(super) fn check(sources: Vec<(PathBuf, String)>, crates_root: &Path) {
    assert!(!sources.is_empty(), "source inventory is empty");
    let sources = sources.into_iter().collect::<BTreeMap<_, _>>();
    let mut assets = BTreeSet::new();
    for entry in std::fs::read_dir(crates_root).expect("read crates directory") {
        let entry = entry.expect("read crate entry");
        if !entry.file_type().expect("read crate entry type").is_dir() {
            continue;
        }
        let dir = entry.path().join("sql");
        match std::fs::metadata(&dir) {
            Ok(metadata) => assert!(
                metadata.is_dir(),
                "SQL path is not a directory: {}",
                dir.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("inspect {}: {e}", dir.display()),
        }
        for entry in std::fs::read_dir(&dir).expect("read SQL directory") {
            let path = entry.expect("read SQL entry").path();
            if path.extension().is_some_and(|e| e == "sql") {
                let sql = std::fs::read_to_string(&path).expect("read SQL asset");
                assert!(
                    !sql.trim().is_empty(),
                    "empty SQL asset: {}",
                    path.display()
                );
                assets.insert(path);
            }
        }
    }
    assert!(!assets.is_empty(), "SQL asset inventory is empty");
    for name in CONVERTED {
        assert!(
            assets.iter().any(|p| p.starts_with(crates_root.join(name))),
            "converted crate {name} contributed no SQL assets"
        );
    }
    let mut errors = reference_errors(&sources, &assets, crates_root);
    errors.extend(inline_errors(
        &sources,
        crates_root,
        CONVERTED,
        INLINE_KEEPS,
    ));
    assert!(
        errors.is_empty(),
        "SQL policy violations:\n{}",
        errors.join("\n")
    );
    let still_inline = still_inline_seen(&sources, &crates_root.join("khive-db"));
    assert!(
        still_inline,
        "must-match control: khive-db no longer trips the inline SQL predicate"
    );
    // Reinsert every extracted statement in all three Rust literal spellings.
    // This is independent of reference discovery and precise inline exceptions.
    let mut round_tripped = 0usize;
    for file in &assets {
        if !CONVERTED
            .iter()
            .any(|name| file.starts_with(crates_root.join(name)))
        {
            continue;
        }
        let statement =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file:?}: {e}"));
        // Headers are not part of the statement rendered back into Rust.
        let body = statement
            .lines()
            .skip_while(|line| {
                let start = line.trim_start();
                start.is_empty() || start.starts_with("--")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.trim().is_empty(),
            "{file:?} holds nothing but comments, so it declares no statement for \
                     the census to protect"
        );
        // A quoted identifier would otherwise close the synthetic literal early
        // and fail this control for a reason that has nothing to do with it.
        let statement = body.trim().replace('"', "\\\"");
        let shapes = [
            (
                "one line",
                statement.split_whitespace().collect::<Vec<_>>().join(" "),
            ),
            ("escaped newlines", statement.replace('\n', "\\n")),
            (
                "line continuations",
                statement.replace('\n', " \\\n            "),
            ),
        ];
        for (shape, rendered) in shapes {
            let snippet = format!("let statement = \"{rendered}\";");
            let seen = sql_literals(&snippet);
            assert_eq!(
                seen.len(),
                1,
                "must-fail control: {file:?} written back into Rust as {shape} was \
                         seen {} time(s), so the census would not notice this statement \
                         moving home",
                seen.len()
            );
            round_tripped += 1;
        }
    }
    assert!(
        round_tripped > 0,
        "must-fail control ran on nothing: {CONVERTED:?} contributed no .sql files, so \
             its passing says only that the loop body never executed"
    );
}

const INLINE_KEEPS: &[Keep] = &[
    Keep { name: "memory_ann_protected_tail", path: "khive-pack-memory/src/ann.rs", sql: "WITH tail AS MATERIALIZED ( SELECT seq, subject_id, op FROM ann_write_log WHERE embedding_model = ?1 AND kind = 'note' AND field = 'note.content' AND seq > ?2 ORDER BY seq LIMIT ?5 ), summary AS MATERIALIZED ( SELECT COUNT(*) AS raw_count, (SELECT MIN(watermark) FROM ann_consumer_watermark WHERE (namespace = ?3 OR namespace = '*') AND embedding_model = ?1) AS min_watermark FROM tail ), selected AS MATERIALIZED ( SELECT seq, subject_id, op FROM tail WHERE (SELECT raw_count FROM summary) <= ?4 ) SELECT 0 AS is_summary, summary.raw_count, summary.min_watermark, NULL AS seq, NULL AS subject_id, NULL AS op, NULL AS vector_model, NULL AS vector_kind, NULL AS vector_field, NULL AS embedding, NULL AS live_note_id FROM summary UNION ALL SELECT 1 AS is_summary, NULL AS raw_count, NULL AS min_watermark, selected.seq, selected.subject_id, selected.op, vectors.embedding_model AS vector_model, vectors.kind AS vector_kind, vectors.field AS vector_field, vectors.embedding, live_note.id AS live_note_id FROM selected LEFT JOIN {table_name} AS vectors ON vectors.subject_id = selected.subject_id LEFT JOIN notes AS live_note ON live_note.id = selected.subject_id AND live_note.deleted_at IS NULL ORDER BY is_summary, seq", reason: "Trusted vector table identifier is embedded in the capped tail and registry snapshot statement." },
    Keep { name: "memory_session_exact_snapshot", path: "khive-pack-memory/src/ann.rs", sql: "WITH session_proof AS MATERIALIZED ( SELECT ({proof}) AS has_fence), {knn_ctes}session_union AS MATERIALIZED ({union}), session_ranked AS MATERIALIZED ( SELECT c.subject_id, c.distance FROM session_union c JOIN notes n ON n.id = c.subject_id AND n.namespace = c.vector_namespace AND n.deleted_at IS NULL ORDER BY c.distance, c.subject_id LIMIT ?2) SELECT p.has_fence, r.subject_id, r.distance FROM session_proof p LEFT JOIN session_ranked r ON 1 = 1 ORDER BY r.distance, r.subject_id", reason: "Fence predicate, per-namespace KNN CTEs and union arms are assembled for one snapshot." },
    Keep { name: "session_union_arm", path: "khive-pack-memory/src/ann.rs", sql: "SELECT subject_id, vector_namespace, distance FROM session_knn_{index}", reason: "Per-namespace CTE identifier contains the runtime index." },
    Keep { name: "population_base_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {base_table} WHERE deleted_at IS NULL", reason: "Base table identifier is selected at runtime." },
    Keep { name: "population_fts_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {fts_table}", reason: "FTS table identifier is selected at runtime." },
    Keep { name: "prune_candidates", path: "khive-pack-memory/src/handlers/prune.rs", sql: "SELECT {projection} FROM notes WHERE kind = 'memory' AND namespace = ? AND deleted_at IS NULL", reason: "Projection depends on the requested pruning mode." },
    Keep { name: "knowledge_compose_section_window", path: "khive-pack-knowledge/src/knowledge/compose.rs", sql: "SELECT id, atom_id, section_type, heading, content, embedding FROM knowledge_sections WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION}", reason: "Variable atom-ID placeholder list and shared servable-section predicate." },
    Keep { name: "knowledge_feedback_target_table", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id FROM {table} WHERE id = ?1 AND deleted_at IS NULL LIMIT 1", reason: "Atom/domain table identifier selected at runtime." },
    Keep { name: "knowledge_domain_offset_projection", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT {select_columns} FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3", reason: "Key-only versus full domain projection selected at runtime." },
    Keep { name: "knowledge_atom_offset_projection_status", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT {select_columns} FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{} ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3", reason: "Selected projection and optional status clause." },
    Keep { name: "knowledge_atom_count_status", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{}", reason: "Optional status clause." },
    Keep { name: "knowledge_delete_domain_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id, slug FROM knowledge_domains", reason: "Incomplete domain projection passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_delete_atom_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id, slug, tags FROM knowledge_atoms", reason: "Incomplete atom projection passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_delete_atom_update_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "UPDATE knowledge_atoms SET deleted_at = ?2", reason: "Incomplete UPDATE passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_cursor_page", path: "khive-pack-knowledge/src/knowledge/cursor_query.rs", sql: "SELECT {columns} FROM {table} WHERE namespace = ?1 AND deleted_at IS NULL {atom_filter}{seek}{status_clause} ORDER BY created_at ASC, id ASC LIMIT {limit}", reason: "Table, projection, status/seek clauses and bound limit assembled at runtime." },
    Keep { name: "knowledge_refusal_target", path: "khive-pack-knowledge/src/knowledge/refusal.rs", sql: "SELECT a.* FROM knowledge_atoms a WHERE {predicate} AND a.deleted_at IS NULL AND NOT EXISTS (SELECT 1 FROM knowledge_domains d WHERE d.id = a.id) LIMIT 1", reason: "ID or namespace/slug predicate selected from the refused input." },
    Keep { name: "knowledge_proto_fts_rowids", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid LIMIT ?2", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_proto_term_frequency", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT count(*) AS frequency FROM ( SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid LIMIT ?2 )", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_fts_scoped_candidates", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT a.* FROM fts_knowledge CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} ORDER BY fts_knowledge.rowid LIMIT ?3", reason: "Status and atom/domain eligibility clauses selected at runtime." },
    Keep { name: "knowledge_proto_fts_scoped_candidates", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT a.* FROM {table} CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid WHERE {table} MATCH ?1 AND +a.namespace = ?2 AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} ORDER BY {table}.rowid LIMIT ?3", reason: "Test-only FTS table and status/type clauses selected at runtime." },
    Keep { name: "knowledge_fts_membership", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT 1 AS present FROM knowledge_atoms WHERE rowid IN ({placeholders}) AND namespace = ?1 LIMIT 1", reason: "Variable rowid placeholder list." },
    Keep { name: "knowledge_proto_namespace_present", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT 1 AS present FROM {table} CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid WHERE {table} MATCH ?1 AND +a.namespace = ?2 LIMIT 1", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_exact_name_eligibility", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT *, CASE WHEN 1{status_clause}{type_clause} THEN 1 ELSE 0 END AS exact_name_eligible FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1", reason: "Status and atom/domain eligibility clauses selected at runtime." },
    Keep { name: "knowledge_hydrate_atoms", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL", reason: "Variable atom-ID placeholder list." },
    Keep { name: "knowledge_hydrate_domains", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT id, slug, name, description, tags, status FROM knowledge_domains WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL", reason: "Variable domain-ID placeholder list." },
    Keep { name: "knowledge_hydrate_phase_b", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT a.*, a.rowid AS rowid FROM knowledge_atoms AS a WHERE a.rowid IN ({rowid_placeholders}) AND +a.namespace = ?1 AND a.deleted_at IS NULL{status_clause}{type_clause}", reason: "Variable rowid placeholders and status/type clauses." },
    Keep { name: "knowledge_domain_member_sizes", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT DISTINCT d.id AS domain_id, a.id AS atom_id, a.name, a.content FROM knowledge_domains AS d LEFT JOIN json_each(d.members) AS member ON 1 = 1 LEFT JOIN knowledge_atoms AS a ON a.namespace = d.namespace AND a.slug = member.value AND a.deleted_at IS NULL WHERE d.namespace = ?1 AND d.id IN ({placeholders}) AND d.deleted_at IS NULL", reason: "Variable domain-ID placeholder list." },
    Keep { name: "knowledge_atom_body_lines", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "SELECT atom_id, SUM(CASE WHEN content = '' THEN 0 ELSE length(content) - length(replace(content, char(10), '')) + (CASE WHEN substr(content, -1) = char(10) THEN 0 ELSE 1 END) END) AS body_lines FROM knowledge_sections WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION} GROUP BY atom_id", reason: "Variable atom-ID placeholders and shared servable-section predicate." },
    Keep { name: "knowledge_compose_atom_window", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "WITH input(ordinal,raw_ref,is_uuid,prefix) AS (VALUES {}) SELECT input.ordinal AS compose_ordinal, CASE WHEN input.is_uuid=0 AND NOT EXISTS( SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) THEN 1 ELSE 0 END AS compose_prefix, atom.* FROM input JOIN knowledge_atoms atom ON atom.rowid IN(SELECT by_id.rowid FROM knowledge_atoms by_id WHERE input.is_uuid=1 AND by_id.id=input.raw_ref AND by_id.namespace=?1 AND by_id.deleted_at IS NULL UNION ALL SELECT by_slug.rowid FROM knowledge_atoms by_slug WHERE input.is_uuid=0 AND by_slug.slug=input.raw_ref AND by_slug.namespace=?1 AND by_slug.deleted_at IS NULL LIMIT 1) OR atom.rowid IN(SELECT prefixed.rowid FROM knowledge_atoms prefixed WHERE input.is_uuid=0 AND input.prefix IS NOT NULL AND NOT EXISTS(SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) AND prefixed.namespace=?1 AND prefixed.deleted_at IS NULL AND prefixed.id LIKE input.prefix LIMIT 2) ORDER BY input.ordinal", reason: "Variable VALUES tuple list carrying per-input ordinals and bind positions." },
    Keep { name: "knowledge_test_low_overlap_seed", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "WITH RECURSIVE x(n) AS ( VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < {x_max} ), y(n) AS ( VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < {y_max} ) INSERT INTO knowledge_atoms ( id, namespace, slug, name, content, tags, properties, finalized, status, source_uri, source_type, created_at, updated_at, deleted_at ) SELECT printf('80000000-0000-0000-0000-%012d', x.n * {y_stride} + y.n), 'local', printf('lowoverlap-%06d', x.n * {y_stride} + y.n), printf('Low Overlap %06d', x.n * {y_stride} + y.n), 'synthetic corpus content entry ' || (x.n * {y_stride} + y.n) || ' discusses topic term' || ((x.n * {y_stride} + y.n) % {vocab_size}) || ' with padding context sentence for realistic length and additional filler', '[]', NULL, 1, 'reviewed', NULL, NULL, x.n * {y_stride} + y.n, x.n * {y_stride} + y.n, NULL FROM x CROSS JOIN y WHERE x.n * {y_stride} + y.n < {n}", reason: "Test-only corpus dimensions, strides, vocabulary and row ceiling interpolated at runtime." },
    Keep { name: "knowledge_challenge_dispute_count", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',coalesce(json_extract(properties,'$.dispute_count'),0)+{affected}) WHERE id=?1 AND namespace=?2", reason: "Affected section count interpolated from the preceding write." },
    Keep { name: "knowledge_resolve_section_status", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "SELECT content_hash FROM knowledge_sections WHERE atom_id=?1 AND section_type=?2 AND {status_filter}", reason: "Caller-selected status predicate." },
    Keep { name: "knowledge_adjudicate_status", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_sections SET status='{new_status}' WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 AND status='disputed'", reason: "New status selected from the adjudication resolution." },
    Keep { name: "knowledge_adjudicate_dispute_count", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',CASE WHEN coalesce(json_extract(properties,'$.dispute_count'),0) >= {affected} THEN coalesce(json_extract(properties,'$.dispute_count'),0)-{affected} ELSE 0 END) WHERE id=?1 AND namespace=?2", reason: "Affected section count interpolated from the preceding write." },
    Keep { name: "knowledge_section_embed_count", path: "khive-pack-knowledge/src/knowledge/sections_index.rs", sql: "SELECT count(*) AS cnt FROM knowledge_sections s JOIN knowledge_atoms a ON a.id = s.atom_id AND a.namespace = s.namespace AND a.deleted_at IS NULL WHERE s.namespace = ?1{atom_filter}{null_filter}", reason: "Optional atom and null-embedding filters." },
    Keep { name: "knowledge_section_embed_page", path: "khive-pack-knowledge/src/knowledge/sections_index.rs", sql: "SELECT s.id AS id, s.heading AS heading, s.content AS content, a.name AS atom_name FROM knowledge_sections s JOIN knowledge_atoms a ON a.id = s.atom_id AND a.namespace = s.namespace AND a.deleted_at IS NULL WHERE s.namespace = ?1{atom_clause}{null_filter}{keyset_clause} ORDER BY s.id LIMIT ?{limit_pos}", reason: "Optional atom/null/keyset clauses and runtime numbered limit position." },
    Keep { name: "knowledge_embedding_coverage", path: "khive-pack-knowledge/src/knowledge/util.rs", sql: "SELECT COUNT(DISTINCT a.id) FROM knowledge_atoms a WHERE a.namespace = ?1 AND a.deleted_at IS NULL AND a.tags NOT LIKE '%type:domain%' AND a.id IN ( SELECT v.subject_id FROM {table_name} v WHERE v.namespace = ?1 AND v.embedding_model = ?2 AND v.field = 'knowledge.atom' )", reason: "Vector table identifier selected from the active model." },
    Keep { name: "ann_raise_authority", path: "khive-retrieval/src/ann/registry.rs", sql: "UPDATE ann_consumer_watermark SET watermark = ?4 WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 AND {predicate}", reason: "Authority selects the predicate." },
    Keep { name: "ann_pending_backfill", path: "khive-retrieval/src/ann/registry.rs", sql: "INSERT OR IGNORE INTO ann_consumer_pending (consumer, namespace, embedding_model, registered_at_us) SELECT watermark.consumer, watermark.namespace, watermark.embedding_model, ?1 FROM ann_consumer_watermark watermark WHERE watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter}", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_expired_select", path: "khive-retrieval/src/ann/registry.rs", sql: "SELECT watermark.consumer, watermark.namespace, watermark.embedding_model, pending.registered_at_us FROM ann_consumer_watermark watermark JOIN ann_consumer_pending pending ON pending.consumer = watermark.consumer AND pending.namespace = watermark.namespace AND pending.embedding_model = watermark.embedding_model WHERE pending.registered_at_us <= ?1 AND watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter} ORDER BY watermark.consumer, watermark.namespace", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_expired_delete", path: "khive-retrieval/src/ann/registry.rs", sql: "DELETE FROM ann_consumer_watermark AS watermark WHERE watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter} AND EXISTS (SELECT 1 FROM ann_consumer_pending pending WHERE pending.consumer = watermark.consumer AND pending.namespace = watermark.namespace AND pending.embedding_model = watermark.embedding_model AND pending.registered_at_us <= ?1)", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_metadata_prune", path: "khive-retrieval/src/ann/registry.rs", sql: "DELETE FROM ann_consumer_pending AS pending WHERE pending.embedding_model = ?2{pending_scope_filter} AND NOT EXISTS (SELECT 1 FROM ann_consumer_watermark watermark WHERE watermark.consumer = pending.consumer AND watermark.namespace = pending.namespace AND watermark.embedding_model = pending.embedding_model AND watermark.watermark = -2)", reason: "Compaction scope adds its aliased namespace predicate." },
    Keep { name: "ann_corpus_fingerprint", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT COUNT(*) AS n FROM {corpus} WHERE {live}", reason: "Trusted vector table, optional live-note join and predicates are selected at runtime." },
    Keep { name: "ann_corpus_scan", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT {columns}, {capture} AS log_s FROM {corpus} WHERE {live} ORDER BY {order}", reason: "Table/join, projection, watermark capture, predicate and ordering are assembled." },
    Keep { name: "ann_corpus_final_tail", path: "khive-retrieval/src/ann/corpus.rs", sql: "WITH {registry_cte}{live_cte}selected AS ( SELECT seq, subject_id, op FROM ann_write_log WHERE {predicate} AND seq > {tail_floor} {selected_order} ), finals AS ( SELECT seq, subject_id, op, first_seq FROM ( SELECT seq, subject_id, op, MIN(seq) OVER (PARTITION BY subject_id) AS first_seq, ROW_NUMBER() OVER ( PARTITION BY subject_id ORDER BY seq DESC ) AS final_rank FROM selected ) WHERE final_rank = 1 ) SELECT {columns} FROM {from} LEFT JOIN {table_name} AS vectors ON vectors.subject_id = finals.subject_id {note_join} ORDER BY finals.first_seq", reason: "Scope, explicit registry policy, optional raw cap and result framing are assembled before one coalesced vector join." },
    Keep { name: "ann_corpus_tail_exists", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log WHERE {predicate} AND seq > ?{seq_param}) AS has_tail", reason: "Namespace/model scope determines predicates and numbered bind position." },
    Keep { name: "ann_corpus_scope_counts", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT (SELECT COUNT(*) FROM {corpus} WHERE {live}) AS live, (SELECT COUNT(*) FROM ann_write_log WHERE {tail} AND seq > ?{seq_param}) AS tail", reason: "Table/join, live predicate, tail predicate and bind position are assembled." },
    Keep { name: "ann_corpus_classification_counts", path: "khive-retrieval/src/ann/corpus.rs", sql: "WITH tail AS MATERIALIZED ( SELECT COUNT(*) AS tail_rows FROM ann_write_log WHERE {tail} AND seq > ?{seq_param} ), cap AS MATERIALIZED ( SELECT CASE WHEN tail_rows = 1 THEN 1 WHEN tail_rows = 0 OR ?{cap_param} IS NULL OR tail_rows > 9223372036854775807 / ?{cap_param} THEN -1 ELSE tail_rows * ?{cap_param} END AS max_rows FROM tail ), live AS ( SELECT COUNT(*) AS live_rows FROM ( SELECT 1 FROM {corpus} WHERE {live} LIMIT (SELECT max_rows FROM cap) ) ) SELECT live.live_rows AS live, tail.tail_rows AS tail, cap.max_rows AS cap FROM live CROSS JOIN tail CROSS JOIN cap", reason: "Table/join, predicates and sequence/cap bind positions are assembled." },
    Keep { name: "replay_atoms_namespace_filter", path: "khive-retrieval/src/replay/engine_replay.rs", sql: "SELECT id FROM atoms WHERE namespace = ? AND id IN ({}) AND deleted_at IS NULL", reason: "Variable candidate-ID placeholder list." },
    Keep { name: "weights_batch_load", path: "khive-retrieval/src/weights/engine_weights.rs", sql: "SELECT atom_id, weight FROM atom_weights WHERE namespace = ?1 AND atom_id IN ({placeholders})", reason: "Variable atom-ID placeholder list, chunked to the bind-parameter limit." },
    Keep { name: "replay_brain_event_by_id", path: "khive-retrieval/src/replay/engine_replay.rs", sql: "SELECT query_text, payload, actor_id, created_at, embedding_model FROM brain_events WHERE id = ?1", reason: "Compiled only under the undeclared `engine` feature; names the pre-event-log brain_events table, so the SQL lint cannot prepare it until the replay reads are retargeted to brain_event_log." },
    Keep { name: "replay_compose_ids_since", path: "khive-retrieval/src/replay/engine_replay.rs", sql: "SELECT id FROM brain_events WHERE kind = 'ComposeEvent' AND created_at >= ?1 AND json_extract(payload, '$.lambda_id') = ?2 ORDER BY created_at DESC", reason: "Compiled only under the undeclared `engine` feature; names the pre-event-log brain_events table, so the SQL lint cannot prepare it until the replay reads are retargeted to brain_event_log." },
    Keep { name: "replay_compose_payloads", path: "khive-retrieval/src/replay/engine_replay.rs", sql: "SELECT payload FROM brain_events WHERE kind = 'ComposeEvent' AND json_extract(payload, '$.lambda_id') = ?1", reason: "Compiled only under the undeclared `engine` feature; names the pre-event-log brain_events table, so the SQL lint cannot prepare it until the replay reads are retargeted to brain_event_log." },
];

fn strip_test_modules(text: &str) -> String {
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

fn sql_literals(text: &str) -> Vec<String> {
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

#[path = "sql_policy_controls.rs"]
mod controls;
