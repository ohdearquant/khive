use super::super::declaration::{
    Acceptance, Reservation, RouteInventoryEntry, RuntimeTableWriteInventoryEntry, StampCapability,
    Substrate, TransactionOwner, WriteClass, ROUTE_INVENTORY, RUNTIME_TABLE_WRITE_INVENTORY,
};
use super::ast_helpers::{literal_string, test_only};
use super::model::{
    DetectedClass, RouteClass, Site, SourcePopulation, NON_PROPERTIES_STORE_METHODS, STORE_WRITES,
    UNCLASSIFIED_SQL, UNRESOLVED_CONSTANT,
};
use super::parsed_sources::{index_module_bindings, parse_production_sources, scan_source};
use super::sql_routes::{
    canonical_key_path, detected_keys, runtime_table_write_count, sql_statements, sql_target,
    sql_unclassified_write, sql_write_occurrences,
};
use super::static_sql_source;
use super::static_sql_source::StaticSqlSources;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use syn::ItemMod;

const FINALIZED_SITE: &str = "khive-runtime/src/secret_gate_finalizer/entity_transaction.rs::PreparedEntityExemption::into_plan";

pub(super) fn is_finalized_candidate_site(site: &Site, row: &RouteInventoryEntry) -> bool {
    site.key == FINALIZED_SITE
        && row.id == "runtime.finalized.entity"
        && row.target == Substrate::Entity
        && row.stamp == StampCapability::AdmissionCapable
        && row.transaction == TransactionOwner::RunAtomicUnit
        && site.calls.contains("entity_insert_if_absent_statement")
        && site.calls.contains("entity_replace_if_unchanged_statement")
}

pub(super) fn scan_migration_sources(sources: &[(String, String)]) -> Vec<Site> {
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

pub(super) fn source_path(path: &Path) -> Option<String> {
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

pub(super) fn module_source_paths(
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

pub(super) fn source_inclusions(
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

pub(super) fn test_module_files(
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

pub(super) fn scan_source_population(
    sources: &[(String, String)],
) -> Result<SourcePopulation, String> {
    scan_source_population_with_sql(sources, &StaticSqlSources::new())
}

pub(super) fn scan_source_population_with_sql(
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
pub(super) fn check_sql_asset_reach(
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

pub(super) fn runtime_table_routes(
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

pub(super) fn scan_sources(sources: &[(String, String)]) -> Result<Vec<Site>, String> {
    let population = scan_source_population(sources)?;
    runtime_table_routes(
        &population,
        ROUTE_INVENTORY,
        RUNTIME_TABLE_WRITE_INVENTORY,
        false,
    )
}

pub(super) fn check_population(
    population: &SourcePopulation,
    inventory: &[RouteInventoryEntry],
    runtime_inventory: &[RuntimeTableWriteInventoryEntry],
    pinned_missing: usize,
) -> Result<(), String> {
    let sites = runtime_table_routes(population, inventory, runtime_inventory, true)?;
    check_inventory(&sites, inventory, pinned_missing)
}

pub(super) fn check_inventory(
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
                Reservation::FinalizedCandidate if is_finalized_candidate_site(site, row) => {}
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

pub(super) fn check_store_trait_methods(sources: &[(String, String)]) -> Result<(), String> {
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

pub(super) fn source_files(
    dir: &Path,
    root: &Path,
    extension: &str,
    output: &mut Vec<(String, String)>,
) {
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

pub(super) fn live_workspace_sql() -> StaticSqlSources {
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

pub(super) fn live_workspace_sources() -> Vec<(String, String)> {
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
