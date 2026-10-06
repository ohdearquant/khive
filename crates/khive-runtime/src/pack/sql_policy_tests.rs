use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use syn::parse::Parser;
use syn::visit::Visit;

const CONVERTED: &[&str] = &[
    "khive-pack-brain",
    "khive-pack-git",
    "khive-pack-kg",
    "khive-pack-memory",
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
            } else {
                scope.unknown_glob = true;
            }
        } else if name == "khive_runtime" && path != "khive_runtime" {
            scope.blocked.insert(name.clone());
        } else if name == "include_str" {
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
}

impl<'ast> Visit<'ast> for References<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.visited.insert(self.path.clone());
        self.scope = scope_for(
            &file.items.iter().collect::<Vec<_>>(),
            self.scope.clone(),
            self.parent.as_ref(),
            self.bindings,
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
        self.scope = scope_for(&items, old.clone(), Some(&old), self.bindings);
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
        errors.push(format!("missing SQL: {}", missing.display()));
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
        MEMORY_KEEPS,
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

const MEMORY_KEEPS: &[Keep] = &[
    Keep { name: "session_union_arm", path: "khive-pack-memory/src/ann.rs", sql: "SELECT subject_id, vector_namespace, distance FROM session_knn_{index}", reason: "Per-namespace CTE identifier contains the runtime index." },
    Keep { name: "population_base_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {base_table} WHERE deleted_at IS NULL", reason: "Base table identifier is selected at runtime." },
    Keep { name: "population_fts_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {fts_table}", reason: "FTS table identifier is selected at runtime." },
    Keep { name: "final_tail_snapshot", path: "khive-pack-memory/src/ann/final_tail.rs", sql: "WITH {live_cte}selected AS ( SELECT seq, subject_id, op FROM ann_write_log WHERE embedding_model = ?1 AND kind = 'note' AND field = 'note.content' AND seq > ?2 {order_limit} ), finals AS ( SELECT seq, subject_id, op, first_seq FROM ( SELECT seq, subject_id, op, MIN(seq) OVER (PARTITION BY subject_id) AS first_seq, ROW_NUMBER() OVER ( PARTITION BY subject_id ORDER BY seq DESC ) AS final_rank FROM selected ) WHERE final_rank = 1 ) SELECT finals.seq, finals.subject_id, finals.op, vectors.embedding_model AS vector_model, vectors.kind AS vector_kind, vectors.field AS vector_field, vectors.embedding, live_note.id AS live_note_id FROM finals LEFT JOIN {table_name} AS vectors ON vectors.subject_id = finals.subject_id LEFT JOIN notes AS live_note ON live_note.id = finals.subject_id AND live_note.deleted_at IS NULL ORDER BY finals.first_seq", reason: "Vector table, optional CTE and order/limit clauses are assembled at runtime." },
    Keep { name: "prune_candidates", path: "khive-pack-memory/src/handlers/prune.rs", sql: "SELECT {projection} FROM notes WHERE kind = 'memory' AND namespace = ? AND deleted_at IS NULL", reason: "Projection depends on the requested pruning mode." },
];

fn strip_test_modules(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(found) = text[cursor..].find("#[cfg(test)]") {
        let start = cursor + found;
        // Only a `mod` item is stripped, not a test-only `use` or `fn`.
        let after = &text[start..];
        let Some(brace_rel) = after.find('{') else {
            out.push_str(&text[cursor..]);
            return out;
        };
        if !after[..brace_rel].contains("mod ") {
            out.push_str(&text[cursor..start + brace_rel]);
            cursor = start + brace_rel;
            continue;
        }
        out.push_str(&text[cursor..start]);
        let mut depth = 0usize;
        let mut i = start + brace_rel;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        cursor = i;
    }
    out.push_str(&text[cursor..]);
    out
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
        ("WITH ", &[" AS ("]),
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
