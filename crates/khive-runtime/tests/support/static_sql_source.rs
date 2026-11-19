use std::collections::BTreeMap;
use std::path::{Component, Path};

pub type StaticSqlSources = BTreeMap<String, String>;

#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedSql {
    pub asset_path: String,
    pub produced_text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanonicalBindings {
    sql_shadowed: bool,
    include_shadowed: bool,
    textual_include_shadowed: bool,
    implicit_prelude_disabled: bool,
    extern_runtime_shadowed: bool,
    canonical_runtime_import: bool,
}

impl CanonicalBindings {
    fn merge(&mut self, other: &Self) {
        self.sql_shadowed |= other.sql_shadowed;
        self.include_shadowed |= other.include_shadowed;
        self.textual_include_shadowed |= other.textual_include_shadowed;
        self.implicit_prelude_disabled |= other.implicit_prelude_disabled;
        self.extern_runtime_shadowed |= other.extern_runtime_shadowed;
        self.canonical_runtime_import |= other.canonical_runtime_import;
    }

    fn module_child(&self) -> Self {
        Self {
            textual_include_shadowed: self.textual_include_shadowed,
            implicit_prelude_disabled: self.implicit_prelude_disabled,
            extern_runtime_shadowed: self.extern_runtime_shadowed,
            canonical_runtime_import: self.canonical_runtime_import,
            ..Self::default()
        }
    }

    pub fn observe_macro(&mut self, item: &syn::ItemMacro) {
        self.textual_include_shadowed |= item.mac.path.is_ident("include");
        self.textual_include_shadowed |= item
            .ident
            .as_ref()
            .is_some_and(|name| name == "include_str");
    }

    pub fn observe_module(&mut self, item: &syn::ItemMod) {
        self.textual_include_shadowed |= item.attrs.iter().any(|a| a.path().is_ident("macro_use"));
    }

    fn import(&mut self, tree: &syn::UseTree, path: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(item) => {
                path.push(item.ident.to_string());
                self.import(&item.tree, path);
                path.pop();
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.import(item, path);
                }
            }
            syn::UseTree::Name(item) => {
                let name = if item.ident == "self" {
                    path.last().cloned().unwrap_or_else(|| "self".into())
                } else {
                    item.ident.to_string()
                };
                self.sql_shadowed |= name == "khive_runtime";
                self.include_shadowed |= name == "include_str";
            }
            syn::UseTree::Rename(item) => {
                self.sql_shadowed |= item.rename == "khive_runtime";
                self.include_shadowed |= item.rename == "include_str";
            }
            syn::UseTree::Glob(_) => {
                self.sql_shadowed = true;
                self.include_shadowed = true;
            }
        }
    }
}

pub fn scope_bindings<'a>(
    items: impl IntoIterator<Item = &'a syn::Item>,
    inherited: &CanonicalBindings,
    test_only: fn(&[syn::Attribute]) -> bool,
) -> CanonicalBindings {
    let mut result = inherited.clone();
    for item in items {
        let (attrs, name) = match item {
            syn::Item::Enum(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Mod(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Struct(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Trait(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::TraitAlias(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Type(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Union(i) => (&i.attrs, Some(&i.ident)),
            syn::Item::Use(i) => (&i.attrs, None),
            syn::Item::Macro(i) => (&i.attrs, None),
            syn::Item::ExternCrate(i) => (&i.attrs, None),
            _ => continue,
        };
        if test_only(attrs) {
            continue;
        }
        result.sql_shadowed |= name.is_some_and(|name| name == "khive_runtime");
        match item {
            syn::Item::Use(i) => result.import(&i.tree, &mut Vec::new()),
            syn::Item::Macro(i) if i.mac.path.is_ident("include") => {
                // include! can inject imports into this same item scope. We
                // inspect SQL loaders, not arbitrary included Rust expansions.
                result.sql_shadowed = true;
                result.include_shadowed = true;
            }
            syn::Item::ExternCrate(i) => {
                let name = i.rename.as_ref().map_or(&i.ident, |(_, name)| name);
                if name == "khive_runtime" {
                    if i.ident == "khive_runtime" {
                        result.canonical_runtime_import = true;
                    } else {
                        result.sql_shadowed = true;
                    }
                }
                result.include_shadowed |= i.attrs.iter().any(|a| a.path().is_ident("macro_use"));
            }
            _ => {}
        }
    }
    result
}

fn no_implicit_prelude(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("no_implicit_prelude")
        || (a.path().is_ident("cfg_attr") && matches!(&a.meta, syn::Meta::List(list) if list.tokens.to_string().contains("no_implicit_prelude"))))
}

pub fn module_bindings(
    item: &syn::ItemMod,
    inherited: &CanonicalBindings,
    test_only: fn(&[syn::Attribute]) -> bool,
) -> CanonicalBindings {
    let mut child = inherited.module_child();
    child.implicit_prelude_disabled |= no_implicit_prelude(&item.attrs);
    item.content.as_ref().map_or(child.clone(), |(_, items)| {
        scope_bindings(items, &child, test_only)
    })
}

pub fn block_bindings(
    block: &syn::Block,
    inherited: &CanonicalBindings,
    test_only: fn(&[syn::Attribute]) -> bool,
) -> CanonicalBindings {
    scope_bindings(
        block.stmts.iter().filter_map(|stmt| match stmt {
            syn::Stmt::Item(item) => Some(item),
            _ => None,
        }),
        inherited,
        test_only,
    )
}

fn path_override(module: &syn::ItemMod) -> Option<String> {
    let attr = module.attrs.iter().find(|a| a.path().is_ident("path"))?;
    let syn::Meta::NameValue(value) = &attr.meta else {
        return None;
    };
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(name),
        ..
    }) = &value.value
    else {
        return None;
    };
    Some(name.value())
}

fn file_module_directory(parent: &str) -> std::path::PathBuf {
    let parent = Path::new(parent);
    let mut base = parent
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    if !matches!(
        parent.file_name().and_then(|name| name.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    ) {
        if let Some(stem) = parent.file_stem() {
            base.push(stem);
        }
    }
    base
}

fn inline_module_directory(
    parent: &str,
    inline_dir: Option<&Path>,
    module: &syn::ItemMod,
) -> std::path::PathBuf {
    if let Some(path) = path_override(module) {
        inline_dir
            .unwrap_or_else(|| Path::new(parent).parent().unwrap_or_else(|| Path::new("")))
            .join(path)
    } else {
        inline_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| file_module_directory(parent))
            .join(module.ident.to_string())
    }
}

fn module_paths(parent: &str, inline_dir: Option<&Path>, module: &syn::ItemMod) -> Vec<String> {
    if let Some(path) = path_override(module) {
        let base = inline_dir
            .unwrap_or_else(|| Path::new(parent).parent().unwrap_or_else(|| Path::new("")));
        return normalized(&base.join(path)).into_iter().collect();
    }
    let base = inline_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file_module_directory(parent))
        .join(module.ident.to_string());
    [base.with_extension("rs"), base.join("mod.rs")]
        .iter()
        .filter_map(|p| normalized(p).ok())
        .collect()
}

fn module_edges(
    parent: &str,
    items: &[syn::Item],
    inline_dir: Option<&Path>,
    inherited: &CanonicalBindings,
    test_only: fn(&[syn::Attribute]) -> bool,
    edges: &mut Vec<(String, String, CanonicalBindings, bool)>,
) {
    let mut bindings = scope_bindings(items, inherited, test_only);
    for item in items {
        match item {
            syn::Item::Mod(module) if !test_only(&module.attrs) => {
                if let Some((_, items)) = &module.content {
                    let directory = inline_module_directory(parent, inline_dir, module);
                    module_edges(
                        parent,
                        items,
                        Some(&directory),
                        &module_bindings(module, &bindings, test_only),
                        test_only,
                        edges,
                    );
                } else {
                    for child in module_paths(parent, inline_dir, module) {
                        edges.push((
                            parent.to_owned(),
                            child,
                            module_bindings(module, &bindings, test_only),
                            false,
                        ));
                    }
                }
                bindings.observe_module(module);
            }
            syn::Item::Macro(item)
                if item.mac.path.is_ident("include") && !test_only(&item.attrs) =>
            {
                if let Ok(name) = syn::parse2::<syn::LitStr>(item.mac.tokens.clone()) {
                    if let Ok(child) = normalized(
                        &Path::new(parent)
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(name.value()),
                    ) {
                        edges.push((parent.to_owned(), child, bindings.clone(), true));
                    }
                }
                bindings.observe_macro(item);
            }
            syn::Item::Macro(item) if !test_only(&item.attrs) => bindings.observe_macro(item),
            _ => {}
        }
    }
}

// Index only lexical ancestors of each parsed file. A sibling's glob/import
// cannot change another module's loader binding. Unknown in-scope globs refuse.
pub fn canonical_bindings(
    files: &[(String, syn::File)],
    test_only: fn(&[syn::Attribute]) -> bool,
) -> BTreeMap<String, CanonicalBindings> {
    let mut result = BTreeMap::new();
    let mut edges = Vec::new();
    for (path, file) in files {
        let mut empty = CanonicalBindings {
            implicit_prelude_disabled: no_implicit_prelude(&file.attrs),
            ..CanonicalBindings::default()
        };
        if matches!(
            Path::new(path).file_name().and_then(|s| s.to_str()),
            Some("lib.rs" | "main.rs")
        ) {
            empty.extern_runtime_shadowed = file.items.iter().any(|item| {
                matches!(item,
                syn::Item::ExternCrate(i) if !test_only(&i.attrs) && i.ident != "khive_runtime"
                    && i.rename.as_ref().is_some_and(|(_, alias)| alias == "khive_runtime"))
            });
        }
        result.insert(path.clone(), scope_bindings(&file.items, &empty, test_only));
        module_edges(path, &file.items, None, &empty, test_only, &mut edges);
    }
    for _ in 0..files.len() {
        let previous = result.clone();
        for (parent, child, edge, include) in &edges {
            if let Some(bindings) = result.get_mut(child) {
                bindings.merge(edge);
                if *include {
                    bindings.merge(&previous[parent]);
                } else {
                    bindings.merge(&previous[parent].module_child());
                }
            }
        }
        if result == previous {
            break;
        }
    }
    result
}

fn normalized(path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir if !parts.is_empty() => {
                parts.pop();
            }
            _ => {
                return Err(format!(
                    "SQL path escapes workspace crates root: {}",
                    path.display()
                ))
            }
        }
    }
    Ok(parts.join("/"))
}

pub fn resolve_static_sql(
    source_path: &str,
    mac: &syn::Macro,
    sources: &StaticSqlSources,
    bindings: &CanonicalBindings,
) -> Result<Option<ResolvedSql>, String> {
    let sql_macro = mac.path.segments.len() == 2
        && mac.path.segments[0].ident == "khive_runtime"
        && mac.path.segments[1].ident == "sql";
    let include = mac.path.is_ident("include_str");
    if !sql_macro && !include {
        return Ok(None);
    }
    let literal = syn::parse2::<syn::LitStr>(mac.tokens.clone());
    let value = match literal {
        Ok(value) => value.value(),
        Err(error) => {
            if sql_macro || mac.tokens.to_string().contains(".sql") {
                return Err(format!("{source_path}: unresolved SQL loader: {error}"));
            }
            return Ok(None);
        }
    };
    if include && Path::new(&value).extension().is_none_or(|ext| ext != "sql") {
        return Ok(None);
    }
    if (sql_macro
        && ((bindings.sql_shadowed && mac.path.leading_colon.is_none())
            || bindings.extern_runtime_shadowed
            || (bindings.implicit_prelude_disabled && !bindings.canonical_runtime_import)))
        || (include
            && (bindings.include_shadowed
                || bindings.textual_include_shadowed
                || bindings.implicit_prelude_disabled))
    {
        return Err(format!(
            "{source_path}: SQL loader has an ambiguous or shadowed binding"
        ));
    }
    let asset_path = if sql_macro {
        if value.is_empty() || value.contains(['/', '\\']) || value == "." || value == ".." {
            return Err(format!("{source_path}: invalid SQL asset name {value:?}"));
        }
        let caller = normalized(Path::new(source_path))?;
        let crate_name = caller.split('/').next().ok_or("missing calling crate")?;
        format!("{crate_name}/sql/{value}.sql")
    } else {
        if value.contains('\\') || Path::new(&value).is_absolute() {
            return Err(format!("{source_path}: invalid SQL include path {value:?}"));
        }
        normalized(
            &Path::new(source_path)
                .parent()
                .ok_or("missing source directory")?
                .join(value),
        )?
    };
    let source = sources
        .get(&asset_path)
        .ok_or_else(|| format!("{source_path}: missing SQL asset {asset_path}"))?;
    Ok(Some(ResolvedSql {
        asset_path,
        produced_text: if sql_macro {
            source.trim_ascii_end().to_owned()
        } else {
            source.clone()
        },
    }))
}

// Opaque macro bodies must not hide a recognized SQL load. vec! is parsed by
// the existing visitors; evaluating concatenation or another macro is outside
// this resolver's contract. Definitions are not application invocations.
fn cursor_punct(
    cursor: syn::buffer::Cursor<'_>,
    expected: char,
) -> Option<syn::buffer::Cursor<'_>> {
    let (punct, rest) = cursor.punct()?;
    (punct.as_char() == expected).then_some(rest)
}

fn sql_path_evidence(mut cursor: syn::buffer::Cursor<'_>) -> bool {
    while !cursor.eof() {
        if let Some((literal, _)) = cursor.literal() {
            if literal.to_string().contains(".sql") {
                return true;
            }
        }
        if let Some((inside, _, _, _)) = cursor.any_group() {
            if sql_path_evidence(inside) {
                return true;
            }
        }
        let Some((_, rest)) = cursor.token_tree() else {
            break;
        };
        cursor = rest;
    }
    false
}

fn loader_tokens(mut cursor: syn::buffer::Cursor<'_>) -> bool {
    while !cursor.eof() {
        if let Some((ident, rest)) = cursor.ident() {
            if ident == "khive_runtime" {
                let qualified = cursor_punct(rest, ':').and_then(|c| cursor_punct(c, ':'));
                if let Some((name, rest)) = qualified.and_then(|c| c.ident()) {
                    if name == "sql"
                        && cursor_punct(rest, '!')
                            .and_then(|c| c.any_group())
                            .is_some()
                    {
                        return true;
                    }
                }
            } else if ident == "include_str" {
                if let Some((arguments, _, _, _)) =
                    cursor_punct(rest, '!').and_then(|c| c.any_group())
                {
                    if sql_path_evidence(arguments) {
                        return true;
                    }
                }
            }
        }
        if let Some((inside, _, _, _)) = cursor.any_group() {
            if loader_tokens(inside) {
                return true;
            }
        }
        let Some((_, rest)) = cursor.token_tree() else {
            break;
        };
        cursor = rest;
    }
    false
}

pub fn opaque_sql_loader(mac: &syn::Macro) -> bool {
    if mac.path.is_ident("vec") || mac.path.is_ident("macro_rules") {
        return false;
    }
    loader_tokens(syn::buffer::TokenBuffer::new2(mac.tokens.clone()).begin())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        expression: &str,
        source: &str,
        sql: &StaticSqlSources,
    ) -> Result<Option<ResolvedSql>, String> {
        let syn::Expr::Macro(expression) = syn::parse_str::<syn::Expr>(expression).unwrap() else {
            panic!("macro fixture")
        };
        let parsed = vec![("sample/src/lib.rs".into(), syn::parse_file(source).unwrap())];
        let bindings = canonical_bindings(&parsed, |_| false);
        let mut bindings = bindings["sample/src/lib.rs"].clone();
        for item in &parsed[0].1.items {
            if let syn::Item::Macro(item) = item {
                bindings.observe_macro(item);
            }
        }
        resolve_static_sql("sample/src/lib.rs", &expression.mac, sql, &bindings)
    }

    #[test]
    fn loaders_preserve_their_distinct_bytes_and_containment() {
        let sql = BTreeMap::from([
            ("sample/sql/query.sql".into(), "SELECT 'a  b'\n \t".into()),
            ("sibling/sql/query.sql".into(), "SELECT 2\n".into()),
        ]);
        assert_eq!(
            resolve("khive_runtime::sql!(\"query\")", "", &sql)
                .unwrap()
                .unwrap()
                .produced_text,
            "SELECT 'a  b'"
        );
        assert_eq!(
            resolve("include_str!(\"../sql/./query.sql\")", "", &sql)
                .unwrap()
                .unwrap()
                .produced_text,
            "SELECT 'a  b'\n \t"
        );
        assert_eq!(
            resolve("include_str!(\"../../sibling/sql/query.sql\")", "", &sql)
                .unwrap()
                .unwrap()
                .asset_path,
            "sibling/sql/query.sql"
        );
        for expression in [
            "include_str!(\"docs.md\")",
            "include_str!(concat!(\"docs\", \".md\"))",
            "include_str!(\"query.sql.txt\")",
        ] {
            assert!(resolve(expression, "", &sql).unwrap().is_none());
        }
        for expression in [
            "khive_runtime::sql!(\"missing\")",
            "khive_runtime::sql!(\"../query\")",
            "khive_runtime::sql!(NAME)",
            "khive_runtime::sql!(\"query\", \"other\")",
            "include_str!(concat!(\"query\", \".sql\"))",
            "include_str!(\"../../../query.sql\")",
            "include_str!(\"/query.sql\")",
        ] {
            assert!(resolve(expression, "", &sql).is_err(), "{expression}");
        }
    }

    #[test]
    fn canonical_bindings_refuse_declarations_imports_and_opaque_imports() {
        let sql = BTreeMap::from([("sample/sql/query.sql".into(), "SELECT 1".into())]);
        for source in [
            "mod khive_runtime {}",
            "use other as khive_runtime;",
            "use other::*;",
        ] {
            assert!(
                resolve("khive_runtime::sql!(\"query\")", source, &sql).is_err(),
                "{source}"
            );
        }
        for source in [
            "macro_rules! include_str { () => {}; }",
            "use other::include_str;",
            "use other::load as include_str;",
            "#[macro_use] extern crate other;",
        ] {
            assert!(
                resolve("include_str!(\"../sql/query.sql\")", source, &sql).is_err(),
                "{source}"
            );
        }
        let parsed = vec![
            (
                "sample/src/lib.rs".into(),
                syn::parse_file("mod child; macro_rules! include_str { () => {}; }").unwrap(),
            ),
            (
                "sample/src/child.rs".into(),
                syn::parse_file("const SQL: &str = include_str!(\"../sql/query.sql\");").unwrap(),
            ),
        ];
        assert!(
            !canonical_bindings(&parsed, |_| false)["sample/src/child.rs"].textual_include_shadowed
        );
        let mut preceding = parsed.clone();
        preceding[0].1 =
            syn::parse_file("macro_rules! include_str { () => {}; } mod child;").unwrap();
        assert!(
            canonical_bindings(&preceding, |_| false)["sample/src/child.rs"]
                .textual_include_shadowed
        );
    }
}
