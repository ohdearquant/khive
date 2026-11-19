//! Library fixtures must send environment writes through the exact-child shim.

use std::path::Path;

#[derive(Default)]
struct EnvironmentWrites {
    shim_file: bool,
    function: String,
    violations: Vec<String>,
    shim_calls: usize,
}

impl EnvironmentWrites {
    fn inspect_path(&mut self, path: &[String]) {
        let Some(last) = path.last() else {
            return;
        };
        if last != "set_var" && last != "remove_var" {
            return;
        }
        let qualified = path.join("::");
        if qualified == format!("crate::test_process::{last}") {
            return;
        }
        if self.shim_file && qualified == format!("std::env::{last}") && self.function == *last {
            self.shim_calls += 1;
            return;
        }
        self.violations
            .push(format!("{}: {qualified}", self.function));
    }

    fn inspect_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        use proc_macro2::TokenTree;
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.inspect_tokens(group.stream()),
                TokenTree::Ident(ident) if ident == "set_var" || ident == "remove_var" => {
                    let mut path = vec![ident.to_string()];
                    let mut position = index;
                    while position >= 3 {
                        match &tokens[position - 3..position] {
                            [TokenTree::Ident(part), TokenTree::Punct(a), TokenTree::Punct(b)]
                                if a.as_char() == ':' && b.as_char() == ':' =>
                            {
                                path.insert(0, part.to_string());
                                position -= 3;
                            }
                            _ => break,
                        }
                    }
                    self.inspect_path(&path);
                }
                _ => {}
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for EnvironmentWrites {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        let previous = std::mem::replace(&mut self.function, function.sig.ident.to_string());
        syn::visit::visit_item_fn(self, function);
        self.function = previous;
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.inspect_path(
            &path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>(),
        );
        syn::visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        let imported = match tree {
            syn::UseTree::Name(name) => Some(&name.ident),
            syn::UseTree::Rename(rename) => Some(&rename.ident),
            _ => None,
        };
        if let Some(imported) = imported {
            if imported == "set_var" || imported == "remove_var" {
                self.violations
                    .push(format!("{}: imported {imported}", self.function));
            }
        }
        syn::visit::visit_use_tree(self, tree);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.inspect_tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

fn inspect(relative: &Path, source: &str) -> EnvironmentWrites {
    use syn::visit::Visit;
    let parsed = syn::parse_file(source).expect("library source must parse");
    let mut writes = EnvironmentWrites {
        shim_file: relative == Path::new("test_process.rs"),
        ..Default::default()
    };
    writes.visit_file(&parsed);
    writes
}

fn walk(root: &Path, directory: &Path, violations: &mut Vec<String>, shim_calls: &mut usize) {
    for entry in std::fs::read_dir(directory).expect("read library source") {
        let entry = entry.expect("source entry");
        let path = entry.path();
        let kind = entry.file_type().expect("source entry type");
        if kind.is_dir() {
            walk(root, &path, violations, shim_calls);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let relative = path.strip_prefix(root).expect("library-relative path");
            let source = std::fs::read_to_string(&path).expect("read Rust source");
            let writes = inspect(relative, &source);
            *shim_calls += writes.shim_calls;
            violations.extend(
                writes
                    .violations
                    .into_iter()
                    .map(|violation| format!("{}: {violation}", relative.display())),
            );
        }
    }
}

#[test]
fn library_environment_writes_use_only_the_isolated_child_shim() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    let mut shim_calls = 0;
    walk(&root, &root, &mut violations, &mut shim_calls);
    assert_eq!(
        shim_calls, 2,
        "exactly one setter and remover live in the shim"
    );
    assert!(
        violations.is_empty(),
        "library environment mutation must use test_process: {violations:#?}"
    );
}

#[test]
fn guard_rejects_aliases_nested_writes_references_and_macro_arguments() {
    for source in [
        "use std::env::set_var as write; fn test() { write(\"K\", \"V\"); }",
        "mod nested { fn test() { std::env::remove_var(\"K\"); } }",
        "fn test() { let setter = std::env::set_var::<&str, &str>; }",
        "fn test() { assert!({ std::env::set_var(\"K\", \"V\"); true }); }",
        "macro_rules! mutate { () => { std::env::set_var(\"K\", \"V\"); } }",
        "fn test() { env::set_var(\"K\", \"V\"); }",
    ] {
        assert!(
            !inspect(Path::new("nested/tests.rs"), source)
                .violations
                .is_empty(),
            "bare mutation escaped the guard: {source}"
        );
    }
}

#[test]
fn guard_ignores_literals_and_accepts_only_the_exact_shim_path() {
    let source = r#"
        // std::env::set_var("K", "V");
        fn test() {
            let example = "std::env::remove_var";
            assert_eq!(example, "std::env::remove_var");
            crate::test_process::set_var("K", "V");
        }
    "#;
    assert!(inspect(Path::new("tests.rs"), source).violations.is_empty());
    let shim = "fn set_var() { std::env::set_var(\"K\", \"V\"); }";
    assert_eq!(inspect(Path::new("test_process.rs"), shim).shim_calls, 1);
    assert!(!inspect(Path::new("nested/test_process.rs"), shim)
        .violations
        .is_empty());
    assert!(!inspect(
        Path::new("test_process.rs"),
        "use std::env::set_var as write; fn unrelated() { write(\"K\", \"V\"); }",
    )
    .violations
    .is_empty());
    assert!(!inspect(
        Path::new("test_process.rs"),
        "mod bypass { use std::env::*; fn mutate() { set_var(\"K\", \"V\"); } }",
    )
    .violations
    .is_empty());
}
