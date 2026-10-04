//! Invocation-local source ASTs shared by both route resolution modes.

use super::*;

pub(super) fn parse_production_sources(
    sources: &[(String, String)],
    skipped: &BTreeSet<String>,
) -> Result<Vec<(String, syn::File)>, String> {
    let mut parsed = Vec::new();
    for (path, source) in sources {
        if skipped.contains(path) || path.contains("/tests/") || path.contains("/benches/") {
            continue;
        }
        parsed.push((
            path.clone(),
            syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?,
        ));
    }
    Ok(parsed)
}

pub(super) fn scan_source(
    path: &str,
    file: &syn::File,
    module_id: ModuleId,
    modules: &ModuleBindings,
    strict: bool,
) -> ScannedSource {
    let parents = module_parents(&module_id, modules);
    let bindings = use_bindings(
        file.items.iter().filter_map(|item| match item {
            syn::Item::Use(item) => Some(item),
            _ => None,
        }),
        &parents,
        None,
        &module_id,
        modules,
        strict,
    );
    let bindings = with_declared_types(bindings, &module_id, modules);
    let mut collector = SourceCollector {
        path: path.to_owned(),
        scope: Vec::new(),
        sites: BTreeMap::new(),
        runtime_tables: BTreeMap::new(),
        all_calls: BTreeMap::new(),
        bindings: vec![bindings],
        parent_module_bindings: parents,
        module_id,
        modules,
        strict,
        path_ordinal: 0,
        constant_references: BTreeMap::new(),
    };
    collector.visit_file(file);
    ScannedSource {
        sites: collector.sites.into_values().collect(),
        runtime_tables: collector.runtime_tables.into_values().collect(),
        constant_references: collector.constant_references,
    }
}

struct ModuleImportTraversal<'a> {
    parsed: &'a BTreeMap<String, &'a syn::File>,
    paths: &'a BTreeSet<String>,
    imports: BTreeMap<ModuleId, Vec<(String, Vec<String>)>>,
    /// The types and values each module declares, by name.
    declared: BTreeMap<ModuleId, SqlBindings>,
    file_modules: BTreeMap<String, ModuleId>,
    visited: BTreeSet<(String, ModuleId)>,
}

fn collect_module_imports(
    path: &str,
    items: &[syn::Item],
    module_id: &ModuleId,
    inline_dirs: &mut Vec<String>,
    traversal: &mut ModuleImportTraversal<'_>,
) {
    if !traversal
        .visited
        .insert((path.to_owned(), module_id.clone()))
    {
        return;
    }
    traversal
        .file_modules
        .entry(path.to_owned())
        .or_insert_with(|| module_id.clone());
    let module_imports = traversal.imports.entry(module_id.clone()).or_default();
    let module_declared = traversal.declared.entry(module_id.clone()).or_default();
    for item in items {
        if let syn::Item::Use(item) = item {
            use_tree_imports(&item.tree, &mut Vec::new(), module_imports);
        } else if let Some((name, binding)) = declared_binding(item) {
            module_declared.insert(name, binding);
        }
    }
    let parsed = traversal.parsed;
    let paths = traversal.paths;
    let parent_dir = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    for item in items {
        match item {
            syn::Item::Mod(module) if !test_only(&module.attrs) => {
                let mut child_module = module_id.clone();
                child_module.segments.push(module.ident.to_string());
                if let Some((_, nested)) = &module.content {
                    inline_dirs.push(module.ident.to_string());
                    collect_module_imports(path, nested, &child_module, inline_dirs, traversal);
                    inline_dirs.pop();
                } else {
                    for child in module_source_paths(path, inline_dirs, module, paths) {
                        if let Some(file) = parsed.get(&child) {
                            collect_module_imports(
                                &child,
                                &file.items,
                                &child_module,
                                &mut Vec::new(),
                                traversal,
                            );
                        }
                    }
                }
            }
            syn::Item::Macro(item)
                if item.mac.path.is_ident("include") && !test_only(&item.attrs) =>
            {
                let Ok(name) = syn::parse2::<syn::LitStr>(item.mac.tokens.clone()) else {
                    continue;
                };
                let Some(child) = source_path(&parent_dir.join(name.value())) else {
                    continue;
                };
                if let Some(file) = parsed.get(&child) {
                    collect_module_imports(
                        &child,
                        &file.items,
                        module_id,
                        &mut Vec::new(),
                        traversal,
                    );
                }
            }
            _ => {}
        }
    }
}

pub(super) fn index_module_bindings(
    sources: &[(String, syn::File)],
    roots: &BTreeSet<String>,
    strict: bool,
) -> (ModuleBindings, BTreeMap<String, ModuleId>) {
    let mut parsed = BTreeMap::new();
    // Module indexing keeps the last entry for a repeated path. Scanning below
    // still visits each original entry, using that entry's own AST.
    for (path, file) in sources {
        parsed.insert(path.clone(), file);
    }
    let paths = parsed.keys().cloned().collect::<BTreeSet<_>>();
    let mut traversal = ModuleImportTraversal {
        parsed: &parsed,
        paths: &paths,
        imports: BTreeMap::new(),
        declared: BTreeMap::new(),
        file_modules: BTreeMap::new(),
        visited: BTreeSet::new(),
    };
    for root in roots {
        if let Some(file) = parsed.get(root) {
            let module_id = ModuleId {
                root: root.clone(),
                segments: Vec::new(),
            };
            collect_module_imports(
                root,
                &file.items,
                &module_id,
                &mut Vec::new(),
                &mut traversal,
            );
        }
    }
    for (path, file) in &parsed {
        if !traversal.file_modules.contains_key(path) {
            let module_id = ModuleId {
                root: path.clone(),
                segments: Vec::new(),
            };
            collect_module_imports(
                path,
                &file.items,
                &module_id,
                &mut Vec::new(),
                &mut traversal,
            );
        }
    }
    let mut modules = traversal
        .imports
        .keys()
        .map(|module_id| (module_id.clone(), SqlBindings::new()))
        .collect::<ModuleBindings>();
    let rounds = traversal.imports.values().map(Vec::len).sum::<usize>() + 1;
    for _ in 0..rounds {
        let previous = modules.clone();
        for (module_id, module_imports) in &traversal.imports {
            let parents = module_parents(module_id, &previous);
            let mut bindings =
                resolve_imports(module_imports, &parents, None, module_id, &previous, strict);
            // An import shadows a declaration of the same name only across
            // namespaces, so the import stays and the declaration fills in.
            for (name, binding) in traversal.declared.get(module_id).into_iter().flatten() {
                bindings.entry(name.clone()).or_insert_with(|| {
                    if module_id.root == "khive-db/src/lib.rs"
                        && module_id.segments == ["stores", "note"]
                        && *binding == Binding::Value
                        && NOTE_PROPERTY_SQL_CONSTANTS.contains(&name.as_str())
                    {
                        Binding::Constant(name.clone())
                    } else {
                        binding.clone()
                    }
                });
            }
            modules.insert(module_id.clone(), bindings);
        }
        if modules == previous {
            break;
        }
    }
    (modules, traversal.file_modules)
}
