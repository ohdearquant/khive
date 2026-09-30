//! The dedicated web receipt writer is a provenance boundary. Keep its
//! production caller population at the web pack's receipt writer.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Item, Meta, Token};

const TARGET: &str = "create_web_receipt_note";
const ALLOWED_FILE: &str = "crates/khive-pack-web/src/receipt.rs";
const ALLOWED_OWNER: &str = "write_receipt";

fn repository_root() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("khive-runtime must have a Cargo workspace parent")
        .to_path_buf();
    assert!(
        workspace.join("Cargo.toml").is_file(),
        "missing Cargo workspace"
    );
    workspace
        .parent()
        .expect("Cargo workspace must have a repository parent")
        .to_path_buf()
}

fn rust_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read source directory {}: {error}", dir.display()))
    {
        let entry = entry.expect("cannot read source directory entry");
        let path = entry.path();
        if path.is_dir() {
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("tests" | "target" | "target-wt")
            ) {
                continue;
            }
            rust_sources(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn cfg_requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let Ok(args) = syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
            else {
                return false;
            };
            if list.path.is_ident("all") {
                args.iter().any(cfg_requires_test)
            } else {
                !args.is_empty() && args.iter().all(cfg_requires_test)
            }
        }
        _ => false,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<Meta>()
                    .is_ok_and(|meta| cfg_requires_test(&meta)))
    })
}

fn item_test_only(item: &Item) -> bool {
    let attrs = match item {
        Item::Const(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        _ => return false,
    };
    test_only(attrs)
}

fn module_dir(file: &Path) -> PathBuf {
    let parent = file.parent().expect("Rust source has a parent directory");
    match file.file_stem().and_then(|stem| stem.to_str()) {
        Some("lib" | "main" | "mod" | "build") => parent.to_path_buf(),
        Some(stem) => parent.join(stem),
        None => panic!("invalid Rust source path: {}", file.display()),
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Default)]
struct TestModuleFiles {
    files: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
}

struct TestModuleFinder<'a> {
    module_dir: PathBuf,
    path_base: PathBuf,
    excluded: &'a mut TestModuleFiles,
}

impl<'ast> Visit<'ast> for TestModuleFinder<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let child_dir = self.module_dir.join(item.ident.to_string());
        if test_only(&item.attrs) {
            self.excluded.dirs.insert(child_dir);
            if item.content.is_none() {
                self.excluded
                    .files
                    .insert(self.module_dir.join(format!("{}.rs", item.ident)));
                for attr in &item.attrs {
                    if attr.path().is_ident("path") {
                        let Meta::NameValue(value) = &attr.meta else {
                            panic!("unsupported #[path] on test module");
                        };
                        let Expr::Lit(literal) = &value.value else {
                            panic!("unsupported #[path] on test module");
                        };
                        let syn::Lit::Str(path) = &literal.lit else {
                            panic!("unsupported #[path] on test module");
                        };
                        let explicit = normalize_path(&self.path_base.join(path.value()));
                        self.excluded.dirs.insert(explicit.with_extension(""));
                        self.excluded.files.insert(explicit);
                    }
                }
            }
            return;
        }
        if item.content.is_some() {
            let old_dir = std::mem::replace(&mut self.module_dir, child_dir.clone());
            let old_base = std::mem::replace(&mut self.path_base, child_dir);
            visit::visit_item_mod(self, item);
            self.path_base = old_base;
            self.module_dir = old_dir;
        }
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
struct CallSite {
    file: String,
    line: usize,
    owner: String,
}

struct CallerScanner {
    file: String,
    owner: String,
    calls: Vec<CallSite>,
}

impl CallerScanner {
    fn record(&mut self, line: usize) {
        self.calls.push(CallSite {
            file: self.file.clone(),
            line,
            owner: self.owner.clone(),
        });
    }

    fn scan_macro_tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Ident(ident) if ident.to_string().trim_start_matches("r#") == TARGET => {
                    // Macro inputs are opaque to syn::Visit. Reject a target
                    // token here even if the macro's expansion is unavailable.
                    self.record(ident.span().start().line);
                }
                TokenTree::Group(group) => self.scan_macro_tokens(group.stream()),
                _ => {}
            }
        }
    }
}

fn named_call(expr: &Expr) -> bool {
    match expr {
        Expr::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == TARGET),
        Expr::Group(group) => named_call(&group.expr),
        Expr::Paren(paren) => named_call(&paren.expr),
        _ => false,
    }
}

impl<'ast> Visit<'ast> for CallerScanner {
    fn visit_item(&mut self, item: &'ast Item) {
        if !item_test_only(item) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.owner = old;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.owner = old;
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.owner, item.sig.ident.to_string());
        visit::visit_trait_item_fn(self, item);
        self.owner = old;
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !test_only(&local.attrs) {
            visit::visit_local(self, local);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if test_only(&call.attrs) {
            return;
        }
        if call.method == TARGET {
            self.record(call.method.span().start().line);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if test_only(&call.attrs) {
            return;
        }
        if named_call(&call.func) {
            self.record(call.func.span().start().line);
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.scan_macro_tokens(mac.tokens.clone());
    }
}

fn call_sites() -> Vec<CallSite> {
    let root = repository_root();
    let expected = root.join(ALLOWED_FILE);
    assert!(
        expected.is_file(),
        "missing expected receipt writer: {}",
        expected.display()
    );
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "Rust source census is empty");

    let mut excluded = TestModuleFiles::default();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        let parsed = syn::parse_file(&text)
            .unwrap_or_else(|error| panic!("cannot parse {}: {error}", file.display()));
        if test_only(&parsed.attrs) {
            excluded.files.insert(file.clone());
            excluded.dirs.insert(module_dir(file));
            continue;
        }
        let mut finder = TestModuleFinder {
            module_dir: module_dir(file),
            path_base: file.parent().expect("source parent").to_path_buf(),
            excluded: &mut excluded,
        };
        finder.visit_file(&parsed);
    }

    let mut calls = Vec::new();
    for file in &files {
        if excluded.files.contains(file) || excluded.dirs.iter().any(|dir| file.starts_with(dir)) {
            continue;
        }
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        if !text.contains(TARGET) {
            continue;
        }
        let parsed = syn::parse_file(&text)
            .unwrap_or_else(|error| panic!("cannot parse {}: {error}", file.display()));
        let relative = file
            .strip_prefix(&root)
            .expect("source lies beneath workspace")
            .to_string_lossy()
            .into_owned();
        let mut scanner = CallerScanner {
            file: relative,
            owner: String::new(),
            calls: Vec::new(),
        };
        scanner.visit_file(&parsed);
        calls.extend(scanner.calls);
    }
    calls.sort();
    calls
}

#[test]
fn only_the_web_receipt_writer_calls_the_dedicated_operation() {
    let calls = call_sites();
    assert!(
        calls.len() == 1 && calls[0].file == ALLOWED_FILE && calls[0].owner == ALLOWED_OWNER,
        "expected exactly one {TARGET} call in {ALLOWED_FILE}::{ALLOWED_OWNER}; found:\n{}",
        calls
            .iter()
            .map(|site| format!("{}:{} in {}", site.file, site.line, site.owner))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn scanner_distinguishes_calls_from_comments_strings_and_test_modules() {
    let parsed = syn::parse_file(
        r#"
fn production(runtime: &Runtime) {
    runtime.create_web_receipt_note();
    Runtime::create_web_receipt_note(runtime);
    let text = "create_web_receipt_note()";
    // runtime.create_web_receipt_note();
}
#[cfg(test)]
mod tests {
    fn ignored(runtime: &Runtime) {
        runtime.create_web_receipt_note();
    }
}
"#,
    )
    .expect("parse caller fixture");
    let mut scanner = CallerScanner {
        file: "fixture.rs".into(),
        owner: String::new(),
        calls: Vec::new(),
    };
    scanner.visit_file(&parsed);
    assert_eq!(scanner.calls.len(), 2);
    assert_eq!(
        scanner
            .calls
            .iter()
            .map(|site| site.line)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(scanner.calls.iter().all(|site| site.owner == "production"));
}

#[test]
fn cfg_test_external_module_is_excluded_by_its_declaration() {
    let parsed = syn::parse_file("#[cfg(test)] #[path = \"sibling_fixture.rs\"] mod fixture;")
        .expect("parse external test module fixture");
    let mut excluded = TestModuleFiles::default();
    let mut finder = TestModuleFinder {
        module_dir: PathBuf::from("src/declaration"),
        path_base: PathBuf::from("src"),
        excluded: &mut excluded,
    };
    finder.visit_file(&parsed);
    assert!(excluded.files.contains(Path::new("src/sibling_fixture.rs")));
}

#[test]
fn macro_input_cannot_hide_a_receipt_caller() {
    let parsed = syn::parse_file(
        "fn production(runtime: &Runtime) { quote!(runtime.create_web_receipt_note()); }",
    )
    .expect("parse macro fixture");
    let mut scanner = CallerScanner {
        file: "fixture.rs".into(),
        owner: String::new(),
        calls: Vec::new(),
    };
    scanner.visit_file(&parsed);
    assert_eq!(scanner.calls.len(), 1);
    assert_eq!(scanner.calls[0].owner, "production");
}
