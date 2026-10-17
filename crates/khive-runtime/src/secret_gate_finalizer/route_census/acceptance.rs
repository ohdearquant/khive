use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use syn::{Attribute, Item, Meta};

use super::super::declaration::{Acceptance, RouteInventoryEntry};
use super::ast_helpers::attribute_name;
use super::source_inventory::{module_source_paths, source_path};

// Unknown platform/features remain possible; cfg(test) is true for these targets.
fn test_cfg(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(true),
        Meta::List(list) => {
            let items = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )
                .ok()?;
            if list.path.is_ident("not") && items.len() == 1 {
                return test_cfg(items.first()?).map(|value| !value);
            }
            let values = items.iter().map(test_cfg).collect::<Vec<_>>();
            if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|value| *value == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn test_attributes(attrs: &[Attribute]) -> Vec<Attribute> {
    let mut expanded = Vec::new();
    for attr in attrs {
        if let Meta::List(list) = &attr.meta {
            if list.path.is_ident("cfg_attr") {
                if let Ok(items) = list.parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                ) {
                    if let Some(condition) = items.first().and_then(test_cfg) {
                        if condition {
                            let nested = items
                                .iter()
                                .skip(1)
                                .map(|meta| syn::parse_quote!(#[#meta]))
                                .collect::<Vec<Attribute>>();
                            expanded.extend(test_attributes(&nested));
                        }
                        continue;
                    }
                }
            }
        }
        expanded.push(attr.clone());
    }
    expanded
}

fn possible_in_test(attrs: &[Attribute]) -> bool {
    !test_attributes(attrs).iter().any(|attr| {
        attr.path().is_ident("cfg")
            && matches!(&attr.meta, Meta::List(list)
                if list.parse_args::<Meta>().is_ok_and(|meta| test_cfg(&meta) == Some(false)))
    })
}

struct AcceptanceTests<'a> {
    sources: BTreeMap<&'a str, &'a str>,
    paths: BTreeSet<String>,
    roots: BTreeSet<String>,
    visited: BTreeSet<(String, String, Vec<String>)>,
    active: BTreeSet<String>,
    tests: BTreeSet<String>,
}

impl AcceptanceTests<'_> {
    fn file(&mut self, path: &str) -> Result<(), String> {
        self.source(path, path, &mut Vec::new())
    }

    fn source(
        &mut self,
        path: &str,
        origin: &str,
        local_modules: &mut Vec<String>,
    ) -> Result<(), String> {
        if self.active.contains(path) {
            return Err(format!("cyclic test source inclusion at {path}"));
        }
        if !self
            .visited
            .insert((path.to_owned(), origin.to_owned(), local_modules.clone()))
        {
            return Ok(());
        }
        self.active.insert(path.to_owned());
        let source = self
            .sources
            .get(path)
            .ok_or_else(|| format!("missing test target source {path}"))?;
        let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
        if possible_in_test(&file.attrs) {
            self.items(path, origin, &file.items, local_modules)?;
        }
        self.active.remove(path);
        Ok(())
    }

    fn items(
        &mut self,
        path: &str,
        origin: &str,
        items: &[Item],
        local_modules: &mut Vec<String>,
    ) -> Result<(), String> {
        for item in items {
            match item {
                Item::Fn(function) if possible_in_test(&function.attrs) => {
                    if test_attributes(&function.attrs)
                        .iter()
                        .any(|attr| matches!(attribute_name(attr).as_str(), "test" | "tokio::test"))
                    {
                        let mut name = local_modules.clone();
                        name.push(function.sig.ident.to_string());
                        self.tests.insert(format!("{origin}::{}", name.join("::")));
                    }
                }
                Item::Mod(module) if possible_in_test(&module.attrs) => {
                    if let Some((_, items)) = &module.content {
                        local_modules.push(module.ident.to_string());
                        self.items(path, origin, items, local_modules)?;
                        local_modules.pop();
                    } else {
                        // Cargo target roots need not be named lib.rs or main.rs.
                        let target_path = Path::new(path)
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join("lib.rs");
                        let parent = if self.roots.contains(path) {
                            target_path.to_str().expect("UTF-8 source path")
                        } else {
                            path
                        };
                        let mut module = module.clone();
                        module.attrs = test_attributes(&module.attrs);
                        let children =
                            module_source_paths(parent, local_modules, &module, &self.paths);
                        if children.len() > 1 {
                            return Err(format!(
                                "{path}: ambiguous source for module {}",
                                module.ident
                            ));
                        }
                        for child in children {
                            self.file(&child)?;
                        }
                    }
                }
                Item::Macro(item)
                    if item.mac.path.is_ident("include") && possible_in_test(&item.attrs) =>
                {
                    if let Ok(name) = syn::parse2::<syn::LitStr>(item.mac.tokens.clone()) {
                        let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
                        if let Some(child) = source_path(&parent.join(name.value())) {
                            if self.paths.contains(&child) {
                                self.source(&child, origin, local_modules)?;
                                self.file(&child)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(super) fn check_acceptance_paths(
    sources: &[(String, String)],
    roots: &BTreeSet<String>,
    inventory: &[RouteInventoryEntry],
) -> Result<(), String> {
    let mut index = AcceptanceTests {
        sources: sources
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect(),
        paths: sources.iter().map(|(path, _)| path.clone()).collect(),
        roots: roots.clone(),
        visited: BTreeSet::new(),
        active: BTreeSet::new(),
        tests: BTreeSet::new(),
    };
    let graph = roots.iter().try_for_each(|root| index.file(root));
    let mut failures = Vec::new();
    for row in inventory {
        let Acceptance::Test { path } = row.acceptance else {
            continue;
        };
        if let Err(error) = &graph {
            failures.push(format!("{}: acceptance {path}: {error}", row.id));
        } else if !index.tests.contains(path) {
            failures.push(format!(
                "{}: acceptance {path} does not resolve to a reachable #[test] or #[tokio::test] at that file-relative module path",
                row.id
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}
