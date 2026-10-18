use super::model::NOTE_PROPERTY_SQL_CONSTANTS;
use std::collections::{BTreeMap, BTreeSet};
use syn::visit::Visit;
use syn::{ItemUse, Pat, PatIdent, UseTree};

/// What a name in scope denotes, as far as the census needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Binding {
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

pub(super) type SqlBindings = BTreeMap<String, Binding>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ModuleId {
    pub(super) root: String,
    pub(super) segments: Vec<String>,
}

pub(super) type ModuleBindings = BTreeMap<ModuleId, SqlBindings>;

/// Name that a `use` tree records for a glob import. It is not an identifier,
/// so it cannot collide with an imported name. A scope binds it to `Other`
/// while it holds a glob import that the census cannot open.
pub(super) const GLOB_IMPORT: &str = "*";

pub(super) fn use_tree_imports(
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
pub(super) fn resolved(binding: Option<&Binding>) -> Option<Binding> {
    match binding {
        Some(binding @ (Binding::Constant(_) | Binding::Module(_))) => Some(binding.clone()),
        _ => None,
    }
}

pub(super) fn import_target(
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
pub(super) fn import_scope_binding(
    first: &str,
    known: &SqlBindings,
    current_module: Option<&SqlBindings>,
) -> Option<Binding> {
    std::iter::once(known)
        .chain(current_module)
        .find_map(|scope| scope.get(first))
        .cloned()
}

pub(super) fn child_module(
    parent: &ModuleId,
    name: &str,
    modules: &ModuleBindings,
) -> Option<ModuleId> {
    let mut child = parent.clone();
    child.segments.push(name.to_owned());
    modules.contains_key(&child).then_some(child)
}

/// The library root of the workspace crate that an extern path names. Paths
/// spell the crate directory `khive-db` as `khive_db`.
pub(super) fn crate_root(name: &str, modules: &ModuleBindings) -> Option<ModuleId> {
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
pub(super) fn resolve_module(
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

pub(super) fn qualified_target(
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
pub(super) fn declared_target(
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
pub(super) fn glob_names(module: &ModuleId, modules: &ModuleBindings) -> Vec<(String, Binding)> {
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

pub(super) fn resolve_imports(
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

pub(super) fn use_bindings<'a>(
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
pub(super) struct BoundNames(pub(super) BTreeSet<String>);

impl<'ast> Visit<'ast> for BoundNames {
    fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
        self.0.insert(pat.ident.to_string());
        syn::visit::visit_pat_ident(self, pat);
    }
}

pub(super) fn shadow_bindings<'a>(patterns: impl Iterator<Item = &'a Pat>) -> SqlBindings {
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

pub(super) fn module_parents(module_id: &ModuleId, modules: &ModuleBindings) -> Vec<SqlBindings> {
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
