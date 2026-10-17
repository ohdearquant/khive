//! Immutable, test=false source-union analysis. It deliberately does not invoke Cargo.
use crate::{scanner, sql_targets};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use syn::{parse::Parser, spanned::Spanned, visit::Visit};

const VIRTUAL: &str = "/architecture-audit-snapshot";
#[derive(Clone, Debug)]
pub(crate) struct Blob {
    pub mode: String,
    pub oid: String,
    pub text: String,
}
#[derive(Default)]
pub(crate) struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub blobs: BTreeMap<String, Blob>,
}
impl Snapshot {
    pub fn logical(path: &str) -> PathBuf {
        Path::new(VIRTUAL).join(path)
    }
    pub fn relative(path: &Path) -> Option<String> {
        path.strip_prefix(VIRTUAL)
            .ok()
            .and_then(|p| p.to_str())
            .map(str::to_owned)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truth {
    False,
    True,
    Unknown,
}
fn meta_args(list: &syn::MetaList) -> Result<Vec<syn::Meta>, String> {
    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .map(|v| v.into_iter().collect())
        .map_err(|e| e.to_string())
}
fn cfg(meta: &syn::Meta, test: bool) -> Result<Truth, String> {
    match meta {
        syn::Meta::Path(path) if path.is_ident("test") => {
            Ok(if test { Truth::True } else { Truth::False })
        }
        syn::Meta::Path(_) | syn::Meta::NameValue(_) => Ok(Truth::Unknown),
        syn::Meta::List(list) => {
            let args = meta_args(list)?;
            let values = args
                .iter()
                .map(|m| cfg(m, test))
                .collect::<Result<Vec<_>, _>>()?;
            if list.path.is_ident("not") {
                if values.len() != 1 {
                    return Err("cfg not requires one predicate".into());
                }
                Ok(match values[0] {
                    Truth::False => Truth::True,
                    Truth::True => Truth::False,
                    Truth::Unknown => Truth::Unknown,
                })
            } else if list.path.is_ident("all") {
                Ok(if values.contains(&Truth::False) {
                    Truth::False
                } else if values.contains(&Truth::Unknown) {
                    Truth::Unknown
                } else {
                    Truth::True
                })
            } else if list.path.is_ident("any") {
                Ok(if values.contains(&Truth::True) {
                    Truth::True
                } else if values.contains(&Truth::Unknown) {
                    Truth::Unknown
                } else {
                    Truth::False
                })
            } else {
                Ok(Truth::Unknown)
            }
        }
    }
}
fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(v) => &v.attrs,
        syn::Item::Enum(v) => &v.attrs,
        syn::Item::ExternCrate(v) => &v.attrs,
        syn::Item::Fn(v) => &v.attrs,
        syn::Item::ForeignMod(v) => &v.attrs,
        syn::Item::Impl(v) => &v.attrs,
        syn::Item::Macro(v) => &v.attrs,
        syn::Item::Mod(v) => &v.attrs,
        syn::Item::Static(v) => &v.attrs,
        syn::Item::Struct(v) => &v.attrs,
        syn::Item::Trait(v) => &v.attrs,
        syn::Item::TraitAlias(v) => &v.attrs,
        syn::Item::Type(v) => &v.attrs,
        syn::Item::Union(v) => &v.attrs,
        syn::Item::Use(v) => &v.attrs,
        _ => &[],
    }
}
fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    match expr {
        syn::Expr::Array(v) => &v.attrs,
        syn::Expr::Assign(v) => &v.attrs,
        syn::Expr::Async(v) => &v.attrs,
        syn::Expr::Await(v) => &v.attrs,
        syn::Expr::Binary(v) => &v.attrs,
        syn::Expr::Block(v) => &v.attrs,
        syn::Expr::Break(v) => &v.attrs,
        syn::Expr::Call(v) => &v.attrs,
        syn::Expr::Cast(v) => &v.attrs,
        syn::Expr::Closure(v) => &v.attrs,
        syn::Expr::Const(v) => &v.attrs,
        syn::Expr::Continue(v) => &v.attrs,
        syn::Expr::Field(v) => &v.attrs,
        syn::Expr::ForLoop(v) => &v.attrs,
        syn::Expr::Group(v) => &v.attrs,
        syn::Expr::If(v) => &v.attrs,
        syn::Expr::Index(v) => &v.attrs,
        syn::Expr::Infer(v) => &v.attrs,
        syn::Expr::Let(v) => &v.attrs,
        syn::Expr::Lit(v) => &v.attrs,
        syn::Expr::Loop(v) => &v.attrs,
        syn::Expr::Macro(v) => &v.attrs,
        syn::Expr::Match(v) => &v.attrs,
        syn::Expr::MethodCall(v) => &v.attrs,
        syn::Expr::Paren(v) => &v.attrs,
        syn::Expr::Path(v) => &v.attrs,
        syn::Expr::Range(v) => &v.attrs,
        syn::Expr::RawAddr(v) => &v.attrs,
        syn::Expr::Reference(v) => &v.attrs,
        syn::Expr::Repeat(v) => &v.attrs,
        syn::Expr::Return(v) => &v.attrs,
        syn::Expr::Struct(v) => &v.attrs,
        syn::Expr::Try(v) => &v.attrs,
        syn::Expr::TryBlock(v) => &v.attrs,
        syn::Expr::Tuple(v) => &v.attrs,
        syn::Expr::Unary(v) => &v.attrs,
        syn::Expr::Unsafe(v) => &v.attrs,
        syn::Expr::While(v) => &v.attrs,
        syn::Expr::Yield(v) => &v.attrs,
        _ => &[],
    }
}
fn pat_attrs(pat: &syn::Pat) -> &[syn::Attribute] {
    match pat {
        syn::Pat::Const(node) => &node.attrs,
        syn::Pat::Ident(node) => &node.attrs,
        syn::Pat::Lit(node) => &node.attrs,
        syn::Pat::Macro(node) => &node.attrs,
        syn::Pat::Or(node) => &node.attrs,
        syn::Pat::Paren(node) => &node.attrs,
        syn::Pat::Path(node) => &node.attrs,
        syn::Pat::Range(node) => &node.attrs,
        syn::Pat::Reference(node) => &node.attrs,
        syn::Pat::Rest(node) => &node.attrs,
        syn::Pat::Slice(node) => &node.attrs,
        syn::Pat::Struct(node) => &node.attrs,
        syn::Pat::Tuple(node) => &node.attrs,
        syn::Pat::TupleStruct(node) => &node.attrs,
        syn::Pat::Type(node) => &node.attrs,
        syn::Pat::Wild(node) => &node.attrs,
        _ => &[],
    }
}
#[derive(Default)]
struct Prepared {
    offset: usize,
    masked: String,
    conditions: Vec<(Range<usize>, String)>,
    overrides: BTreeMap<usize, String>,
    diagnostics: Vec<String>,
    excluded: Vec<Range<usize>>,
}
struct Masker {
    prepared: Prepared,
    blank: Vec<Range<usize>>,
    test: bool,
}
impl Masker {
    fn meta(&mut self, meta: &syn::Meta, range: Range<usize>) -> bool {
        if let syn::Meta::List(list) = meta {
            if list.path.is_ident("cfg") {
                match syn::parse2::<syn::Meta>(list.tokens.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|m| cfg(&m, self.test))
                {
                    Ok(Truth::False) => {
                        self.blank.push(range.clone());
                        self.prepared.excluded.push(range);
                        return false;
                    }
                    Ok(Truth::Unknown) => self
                        .prepared
                        .conditions
                        .push((range, format!("cfg({})", list.tokens))),
                    Ok(Truth::True) => {}
                    Err(e) => self
                        .prepared
                        .diagnostics
                        .push(format!("malformed cfg: {e}")),
                }
            } else if list.path.is_ident("cfg_attr") {
                match meta_args(list) {
                    Ok(args) if !args.is_empty() => match cfg(&args[0], self.test) {
                        Ok(Truth::False) => {}
                        Ok(Truth::True) => {
                            for attr in &args[1..] {
                                if !self.meta(attr, range.clone()) {
                                    return false;
                                }
                            }
                        }
                        Ok(Truth::Unknown) => {
                            if args[1..].iter().any(|a| {
                                a.path().is_ident("path")
                                    || a.path().is_ident("cfg")
                                    || a.path().is_ident("cfg_attr")
                            }) {
                                self.prepared
                                    .conditions
                                    .push((range.clone(), format!("cfg_attr({})", list.tokens)));
                                self.prepared.diagnostics.push("conditional cfg_attr changes path/cfg; alternatives require explicit review".into());
                            }
                        }
                        Err(e) => self.prepared.diagnostics.push(e),
                    },
                    _ => self.prepared.diagnostics.push("malformed cfg_attr".into()),
                }
            }
        } else if let syn::Meta::NameValue(value) = meta {
            if value.path.is_ident("path") {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(path),
                    ..
                }) = &value.value
                {
                    if self
                        .prepared
                        .overrides
                        .insert(range.start, path.value())
                        .is_some_and(|old| old != path.value())
                    {
                        self.prepared
                            .diagnostics
                            .push("multiple effective #[path] values".into());
                    }
                } else {
                    self.prepared.diagnostics.push("nonliteral #[path]".into());
                }
            }
        }
        true
    }
    fn attrs(&mut self, attrs: &[syn::Attribute], span: proc_macro2::Span) -> bool {
        let range = span.byte_range();
        for attr in attrs {
            if !self.meta(&attr.meta, range.clone()) {
                return false;
            }
        }
        true
    }
}
impl<'ast> Visit<'ast> for Masker {
    fn visit_file(&mut self, file: &'ast syn::File) {
        // Inner file cfg applies to the entire file, including all children.
        let range = 0..self
            .prepared
            .masked
            .len()
            .saturating_sub(self.prepared.offset);
        for attr in &file.attrs {
            if !self.meta(&attr.meta, range.clone()) {
                return;
            }
        }
        syn::visit::visit_file(self, file);
    }
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if self.attrs(item_attrs(item), item.span()) {
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        if self.attrs(expr_attrs(expr), expr.span()) {
            syn::visit::visit_expr(self, expr);
        }
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if self.attrs(&local.attrs, local.span()) {
            syn::visit::visit_local(self, local);
        }
    }
    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_stmt_macro(self, node);
        }
    }
    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_arm(self, node);
        }
    }
    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_field_value(self, node);
        }
    }
    fn visit_field(&mut self, node: &'ast syn::Field) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_field(self, node);
        }
    }
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_variant(self, node);
        }
    }
    fn visit_pat_type(&mut self, node: &'ast syn::PatType) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_pat_type(self, node);
        }
    }
    fn visit_const_param(&mut self, node: &'ast syn::ConstParam) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_const_param(self, node);
        }
    }
    fn visit_type_param(&mut self, node: &'ast syn::TypeParam) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_type_param(self, node);
        }
    }
    fn visit_lifetime_param(&mut self, node: &'ast syn::LifetimeParam) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_lifetime_param(self, node);
        }
    }
    fn visit_bare_fn_arg(&mut self, node: &'ast syn::BareFnArg) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_bare_fn_arg(self, node);
        }
    }
    fn visit_bare_variadic(&mut self, node: &'ast syn::BareVariadic) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_bare_variadic(self, node);
        }
    }
    fn visit_receiver(&mut self, node: &'ast syn::Receiver) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_receiver(self, node);
        }
    }
    fn visit_variadic(&mut self, node: &'ast syn::Variadic) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_variadic(self, node);
        }
    }
    fn visit_field_pat(&mut self, node: &'ast syn::FieldPat) {
        if self.attrs(&node.attrs, node.span()) {
            syn::visit::visit_field_pat(self, node);
        }
    }
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        if self.attrs(pat_attrs(node), node.span()) {
            syn::visit::visit_pat(self, node);
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Const(v) => &v.attrs,
            syn::ImplItem::Fn(v) => &v.attrs,
            syn::ImplItem::Type(v) => &v.attrs,
            syn::ImplItem::Macro(v) => &v.attrs,
            _ => return,
        };
        if self.attrs(attrs, item.span()) {
            syn::visit::visit_impl_item(self, item);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs = match item {
            syn::TraitItem::Const(v) => &v.attrs,
            syn::TraitItem::Fn(v) => &v.attrs,
            syn::TraitItem::Type(v) => &v.attrs,
            syn::TraitItem::Macro(v) => &v.attrs,
            _ => return,
        };
        if self.attrs(attrs, item.span()) {
            syn::visit::visit_trait_item(self, item);
        }
    }
    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        let attrs = match item {
            syn::ForeignItem::Fn(v) => &v.attrs,
            syn::ForeignItem::Static(v) => &v.attrs,
            syn::ForeignItem::Type(v) => &v.attrs,
            syn::ForeignItem::Macro(v) => &v.attrs,
            _ => return,
        };
        if self.attrs(attrs, item.span()) {
            syn::visit::visit_foreign_item(self, item);
        }
    }
}
fn prepare(text: &str, test: bool) -> Prepared {
    let file = match syn::parse_file(text) {
        Ok(file) => file,
        Err(e) => {
            return Prepared {
                masked: text.into(),
                diagnostics: vec![format!("parse Rust: {e}")],
                ..Prepared::default()
            }
        }
    };
    let offset = usize::from(text.starts_with('\u{feff}')) * 3
        + file.shebang.as_ref().map_or(0, String::len);
    let mut masker = Masker {
        prepared: Prepared {
            offset,
            masked: text.into(),
            ..Prepared::default()
        },
        blank: Vec::new(),
        test,
    };
    masker.visit_file(&file);
    let mut bytes = text.as_bytes().to_vec();
    for range in masker.blank {
        if let Some(slice) = bytes.get_mut(offset + range.start..offset + range.end) {
            for byte in slice {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        } else {
            masker
                .prepared
                .diagnostics
                .push("cfg span outside original source".into());
        }
    }
    masker.prepared.masked = String::from_utf8(bytes).expect("masking preserves UTF-8");
    masker.prepared
}
fn confined(path: &Path) -> bool {
    path.starts_with(VIRTUAL) && Snapshot::relative(path).is_some()
}
fn manifest_roots(snapshot: &Snapshot, name: &str) -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    let base = format!("crates/{name}");
    let manifest = format!("{base}/Cargo.toml");
    let blob = snapshot
        .blobs
        .get(&manifest)
        .ok_or_else(|| format!("missing crate {name} manifest"))?;
    let cargo = blob
        .text
        .parse::<toml::Value>()
        .map_err(|e| format!("{manifest}: {e}"))?;
    let package = cargo
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| format!("{manifest}: package table required"))?;
    let boolean = |key: &str| -> Result<bool, String> {
        package.get(key).map_or(Ok(true), |v| {
            v.as_bool()
                .ok_or_else(|| format!("{manifest}: {key} must be boolean"))
        })
    };
    let mut roots = BTreeMap::new();
    let mut add = |target: &toml::Value, default: Option<String>| -> Result<(), String> {
        let table = target
            .as_table()
            .ok_or_else(|| format!("{manifest}: target must be a table"))?;
        let path = match table.get("path") {
            Some(v) => v
                .as_str()
                .ok_or_else(|| format!("{manifest}: target path must be string"))?
                .to_owned(),
            None => {
                default.ok_or_else(|| format!("{manifest}: target has no discoverable path"))?
            }
        };
        let full = scanner::normalize(&Snapshot::logical(&base).join(path));
        if !confined(&full) {
            return Err(format!("{manifest}: target escapes snapshot"));
        }
        let relative = Snapshot::relative(&full).unwrap();
        if !snapshot.blobs.contains_key(&relative) {
            return Err(format!("{manifest}: missing declared root {relative}"));
        }
        let mut features = Vec::new();
        if let Some(value) = table.get("required-features") {
            for feature in value
                .as_array()
                .ok_or_else(|| format!("{manifest}: required-features must be array"))?
            {
                features.push(format!(
                    "required-feature={}",
                    feature
                        .as_str()
                        .ok_or_else(|| format!("{manifest}: feature must be string"))?
                ));
            }
        }
        if table.is_empty() && roots.keys().any(|(path, _)| path == &full) {
            return Ok(());
        }
        roots.insert((full, features), ());
        Ok(())
    };
    if let Some(lib) = cargo.get("lib") {
        add(lib, Some("src/lib.rs".into()))?;
    } else if boolean("autolib")? && snapshot.blobs.contains_key(&format!("{base}/src/lib.rs")) {
        add(
            &toml::Value::Table(Default::default()),
            Some("src/lib.rs".into()),
        )?;
    }
    let mut named = BTreeSet::new();
    if let Some(bins) = cargo.get("bin") {
        for bin in bins
            .as_array()
            .ok_or_else(|| format!("{manifest}: bin must be array of tables"))?
        {
            let bin_name = bin
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{manifest}: bin name required"))?;
            named.insert(bin_name.to_owned());
            let candidates = [
                format!("src/bin/{bin_name}.rs"),
                format!("src/bin/{bin_name}/main.rs"),
                "src/main.rs".into(),
            ];
            let available = candidates
                .into_iter()
                .filter(|p| snapshot.blobs.contains_key(&format!("{base}/{p}")))
                .collect::<Vec<_>>();
            if bin.get("path").is_none() && available.len() > 1 {
                return Err(format!(
                    "{manifest}: ambiguous inferred bin {bin_name}; explicit path required"
                ));
            }
            add(bin, available.into_iter().next())?;
        }
    }
    if boolean("autobins")? {
        if snapshot.blobs.contains_key(&format!("{base}/src/main.rs")) {
            add(
                &toml::Value::Table(Default::default()),
                Some("src/main.rs".into()),
            )?;
        }
        let prefix = format!("{base}/src/bin/");
        let mut automatic_names = BTreeSet::new();
        for path in snapshot.blobs.keys().filter(|p| p.starts_with(&prefix)) {
            let tail = &path[prefix.len()..];
            let candidate = if !tail.contains('/') && tail.ends_with(".rs") {
                Some((tail.trim_end_matches(".rs"), format!("src/bin/{tail}")))
            } else {
                tail.strip_suffix("/main.rs")
                    .filter(|n| !n.contains('/'))
                    .map(|n| (n, format!("src/bin/{tail}")))
            };
            if let Some((bin, path)) = candidate {
                if !named.contains(bin) {
                    if !automatic_names.insert(bin.to_owned()) {
                        return Err(format!("{manifest}: ambiguous automatic bin {bin}"));
                    }
                    add(&toml::Value::Table(Default::default()), Some(path))?;
                }
            }
        }
    }
    if roots.is_empty() {
        return Err(format!("{manifest}: no production lib/bin roots"));
    }
    Ok(roots.into_keys().collect())
}
fn site_json(
    site: &scanner::Site,
    info: &BTreeMap<PathBuf, Prepared>,
    roots: &[String],
    sources: &BTreeMap<PathBuf, String>,
) -> Value {
    let offset = info.get(&site.path).map_or(0, |p| p.offset);
    let start = offset + site.range.start;
    let end = offset + site.range.end;
    let mut conditions = roots.to_vec();
    let own_site = (site.path.clone(), site.range.clone());
    for (path, range) in site.mounts.iter().chain(std::iter::once(&own_site)) {
        if let Some(prepared) = info.get(path) {
            for (conditional, label) in &prepared.conditions {
                if conditional.start <= range.start && conditional.end >= range.end {
                    conditions.push(format!(
                        "{}:{label}",
                        Snapshot::relative(path).unwrap_or_default()
                    ));
                }
            }
        }
    }
    conditions.sort();
    conditions.dedup();
    let text = sources.get(&site.path).map(String::as_str).unwrap_or("");
    let prefix = text.get(..start).unwrap_or("");
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    json!({"path":Snapshot::relative(&site.path),"start":start,"end":end,"line":line,"column":column,"definite":conditions.is_empty(),"cfg":conditions,"kind":site.kind,"value":site.value})
}
fn merge_site(
    rows: &mut BTreeMap<(String, usize, usize, String), Value>,
    row: Value,
    discriminator: String,
) {
    let key = (
        row["path"].as_str().unwrap_or("").into(),
        row["start"].as_u64().unwrap_or(0) as usize,
        row["end"].as_u64().unwrap_or(0) as usize,
        discriminator,
    );
    if let Some(old) = rows.get_mut(&key) {
        old["definite"] = json!(old["definite"] == true || row["definite"] == true);
        let cfg = row["cfg"].as_array().cloned().unwrap_or_default();
        let old_cfg = old["cfg"].as_array_mut().unwrap();
        for condition in cfg {
            if !old_cfg.contains(&condition) {
                old_cfg.push(condition);
            }
        }
        old_cfg.sort_by_key(Value::to_string);
        let provenance = row["root"].clone();
        if !old["roots"].as_array().unwrap().contains(&provenance) {
            old["roots"].as_array_mut().unwrap().push(provenance);
        }
    } else {
        let mut row = row;
        row["roots"] = json!([row["root"].clone()]);
        rows.insert(key, row);
    }
}
pub(crate) fn analyze(snapshot: &Snapshot, selected: &[String]) -> Value {
    let root = Snapshot::logical("crates");
    let all_crates = snapshot
        .blobs
        .keys()
        .filter_map(|p| p.strip_prefix("crates/")?.strip_suffix("/Cargo.toml"))
        .filter(|n| !n.contains('/'))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let names = if selected.is_empty() {
        all_crates.clone()
    } else {
        selected.iter().cloned().collect()
    };
    let sources = snapshot
        .blobs
        .iter()
        .filter(|(p, _)| p.ends_with(".rs"))
        .map(|(p, b)| (Snapshot::logical(p), b.text.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut info = BTreeMap::new();
    let mut parsed = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    let mut excluded = BTreeMap::new();
    for (path, text) in &sources {
        let prepared = prepare(text, false);
        if let Ok(file) = syn::parse_file(text) {
            parsed.insert(path.clone(), file);
        }
        for (start, target) in &prepared.overrides {
            overrides.insert((path.clone(), *start), target.clone());
        }
        excluded.insert(path.clone(), prepared.excluded.clone());
        info.insert(path.clone(), prepared);
    }
    let assets = snapshot
        .blobs
        .keys()
        .filter(|p| {
            let parts = p.split('/').collect::<Vec<_>>();
            parts.len() == 4 && parts[0] == "crates" && parts[2] == "sql" && p.ends_with(".sql")
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut test_info = BTreeMap::new();
    let mut test_parsed = BTreeMap::new();
    let mut test_overrides = BTreeMap::new();
    let mut test_excluded = BTreeMap::new();
    for (path, text) in &sources {
        let prepared = prepare(text, true);
        if let Ok(file) = syn::parse_file(text) {
            test_parsed.insert(path.clone(), file);
        }
        for (start, target) in &prepared.overrides {
            test_overrides.insert((path.clone(), *start), target.clone());
        }
        test_excluded.insert(path.clone(), prepared.excluded.clone());
        test_info.insert(path.clone(), prepared);
    }
    let mut diagnostics = Vec::new();
    let mut reports = Vec::new();
    let mut global_used = BTreeSet::new();
    let mut global_test_used = BTreeSet::new();
    let mut global_definite = BTreeSet::new();
    let mut test_coverage_incomplete = false;
    // Analyze all selected source roots first, including inline-only crates.
    for name in &names {
        if !all_crates.contains(name) {
            diagnostics.push(format!("absent requested crate {name}"));
            continue;
        }
        let entries = match manifest_roots(snapshot, name) {
            Ok(entries) => entries,
            Err(e) => {
                diagnostics.push(e);
                test_coverage_incomplete = true;
                continue;
            }
        };
        let mut readers = BTreeMap::new();
        let mut literals = BTreeMap::new();
        let mut visited = BTreeSet::new();
        let mut local_errors = Vec::new();
        for (entry, features) in &entries {
            let observed = scanner::observe(
                &sources,
                &parsed,
                &root,
                &root.join(name),
                entry,
                &overrides,
                &excluded,
            );
            local_errors.extend(observed.diagnostics);
            for path in observed.requested {
                if let Some(prepared) = info.get(&path) {
                    for error in &prepared.diagnostics {
                        local_errors.push(format!(
                            "{}: {error}",
                            Snapshot::relative(&path).unwrap_or_default()
                        ));
                    }
                }
            }
            visited.extend(observed.visited);
            for site in observed.readers {
                let asset = Snapshot::relative(Path::new(&site.value));
                let Some(asset) = asset.filter(|p| {
                    snapshot
                        .blobs
                        .get(p)
                        .is_some_and(|b| b.mode == "100644" || b.mode == "100755")
                }) else {
                    local_errors.push(format!(
                        "{}: missing or escaping immutable SQL asset {}",
                        site.path.display(),
                        site.value
                    ));
                    continue;
                };
                if snapshot.blobs[&asset].text.trim().is_empty() {
                    local_errors.push(format!("{asset}: empty SQL asset"));
                }
                let asset_analysis = sql_targets::analyze(&snapshot.blobs[&asset].text);
                for error in asset_analysis.diagnostics {
                    local_errors.push(format!("{asset}: {error}"));
                }
                global_used.insert(asset.clone());
                let mut row = site_json(&site, &info, features, &sources);
                row["asset"] = json!(asset);
                row["blob"] = json!(snapshot.blobs[&asset].oid);
                row["root"] = json!(Snapshot::relative(entry));
                if row["definite"] == true {
                    global_definite.insert(asset.clone());
                }
                merge_site(&mut readers, row, format!("{asset}:{}", site.kind));
            }
            for site in observed.literals {
                let analysis = if site.kind == "format-template" {
                    sql_targets::analyze_template(&site.value)
                } else {
                    sql_targets::analyze(&site.value)
                };
                if !analysis.statement && analysis.diagnostics.is_empty() {
                    continue;
                }
                let mut row = site_json(&site, &info, features, &sources);
                row["root"] = json!(Snapshot::relative(entry));
                row["targets"] = json!(analysis
                    .targets
                    .iter()
                    .map(|t| json!({"relation":t.relation,"role":t.role,"dynamic":t.dynamic}))
                    .collect::<Vec<_>>());
                row["diagnostics"] = json!(analysis.diagnostics);
                row["tableless"] = json!(analysis.targets.is_empty());
                for error in row["diagnostics"].as_array().unwrap() {
                    local_errors.push(format!(
                        "{}:{}:{}: {}",
                        row["path"], row["line"], row["column"], error
                    ));
                }
                merge_site(&mut literals, row, "literal".into());
            }
        }
        for path in &visited {
            if let Some(prepared) = info.get(path) {
                for error in &prepared.diagnostics {
                    local_errors.push(format!(
                        "{}: {error}",
                        Snapshot::relative(path).unwrap_or_default()
                    ));
                }
            }
        }
        // Separate test=true source-union evidence never becomes a production root or credit.
        let mut test_entries = entries
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<BTreeSet<_>>();
        let mut test_diagnostics = Vec::new();
        let manifest = &snapshot.blobs[&format!("crates/{name}/Cargo.toml")].text;
        if let Ok(cargo) = manifest.parse::<toml::Value>() {
            if cargo
                .get("package")
                .and_then(|p| p.get("autotests"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(true)
            {
                let prefix = format!("crates/{name}/tests/");
                for path in sources.keys().filter_map(|p| Snapshot::relative(p)) {
                    if let Some(tail) = path.strip_prefix(&prefix) {
                        if (!tail.contains('/') && tail.ends_with(".rs"))
                            || tail
                                .strip_suffix("/main.rs")
                                .is_some_and(|n| !n.contains('/'))
                        {
                            test_entries.insert(Snapshot::logical(&path));
                        }
                    }
                }
            }
            if cargo
                .get("package")
                .and_then(|p| p.get("autotests"))
                .is_some_and(|v| v.as_bool().is_none())
            {
                test_diagnostics.push("Cargo autotests must be boolean".into());
            }
            if let Some(value) = cargo.get("test") {
                if let Some(tests) = value.as_array() {
                    for test in tests {
                        let path =
                            if let Some(path) = test.get("path").and_then(toml::Value::as_str) {
                                Some(path.to_owned())
                            } else if let Some(test_name) =
                                test.get("name").and_then(toml::Value::as_str)
                            {
                                let candidates = [
                                    format!("tests/{test_name}.rs"),
                                    format!("tests/{test_name}/main.rs"),
                                ];
                                let matches = candidates
                                    .into_iter()
                                    .filter(|p| {
                                        snapshot.blobs.contains_key(&format!("crates/{name}/{p}"))
                                    })
                                    .collect::<Vec<_>>();
                                if matches.len() != 1 {
                                    test_diagnostics.push(format!(
                                        "missing or ambiguous declared test {test_name}"
                                    ));
                                    None
                                } else {
                                    matches.into_iter().next()
                                }
                            } else {
                                test_diagnostics.push("declared test needs a path or name".into());
                                None
                            };
                        if let Some(path) = path {
                            let path = scanner::normalize(&root.join(name).join(path));
                            if confined(&path) {
                                test_entries.insert(path);
                            } else {
                                test_diagnostics.push("test root escapes snapshot".into());
                            }
                        }
                    }
                } else {
                    test_diagnostics.push("Cargo test must be an array of tables".into());
                }
            }
        }
        let mut test_readers = BTreeMap::new();
        for entry in test_entries {
            let observed = scanner::observe(
                &sources,
                &test_parsed,
                &root,
                &root.join(name),
                &entry,
                &test_overrides,
                &test_excluded,
            );
            test_diagnostics.extend(observed.diagnostics);
            for path in observed.visited.iter().chain(&observed.requested) {
                if let Some(prepared) = test_info.get(path) {
                    for error in &prepared.diagnostics {
                        test_diagnostics.push(format!(
                            "{}: {error}",
                            Snapshot::relative(path).unwrap_or_default()
                        ));
                    }
                }
            }
            for site in observed.readers {
                if let Some(asset) = Snapshot::relative(Path::new(&site.value))
                    .filter(|p| snapshot.blobs.contains_key(p))
                {
                    let mut row = site_json(&site, &test_info, &[], &sources);
                    row["asset"] = json!(asset);
                    row["root"] = json!(Snapshot::relative(&entry));
                    global_test_used.insert(asset.clone());
                    merge_site(&mut test_readers, row, format!("{asset}:{}", site.kind));
                } else {
                    test_diagnostics.push(format!(
                        "{}: missing or escaping immutable test SQL asset {}",
                        site.path.display(),
                        site.value
                    ));
                }
            }
        }
        test_diagnostics.sort();
        test_diagnostics.dedup();
        let test_source_complete = test_diagnostics.is_empty();
        test_coverage_incomplete |= !test_source_complete;
        let inline = literals
            .values()
            .filter(|v| !v["targets"].as_array().unwrap().is_empty())
            .count();
        let inline_definite = literals
            .values()
            .filter(|v| !v["targets"].as_array().unwrap().is_empty() && v["definite"] == true)
            .count();
        let readers_definite = readers.values().filter(|v| v["definite"] == true).count();
        let tableless = literals
            .values()
            .filter(|v| {
                v["targets"].as_array().unwrap().is_empty()
                    && v["diagnostics"].as_array().unwrap().is_empty()
            })
            .count();
        let unmounted = sources
            .keys()
            .filter(|p| p.starts_with(root.join(name)) && !visited.contains(*p))
            .filter_map(|p| Snapshot::relative(p))
            .collect::<Vec<_>>();
        let test_only_source = info.iter().filter(|(p, i)| p.starts_with(root.join(name)) && !i.excluded.is_empty()).map(|(p, i)| json!({"path":Snapshot::relative(p),"excluded_ranges":i.excluded.iter().map(|r|json!([i.offset+r.start,i.offset+r.end])).collect::<Vec<_>>()})).collect::<Vec<_>>();
        local_errors.sort();
        local_errors.dedup();
        reports.push(json!({"crate":name,"manifest":format!("crates/{name}/Cargo.toml"),"roots":entries.iter().map(|(p,f)|json!({"path":Snapshot::relative(p),"required_features":f})).collect::<Vec<_>>(),"test_source_complete":test_source_complete,"test_source_diagnostics":test_diagnostics,"reader_sites_definite":readers_definite,"reader_sites_conditional":readers.len()-readers_definite,"inline_definite":inline_definite,"inline_conditional":inline-inline_definite,"test_source_reader_sites":test_readers.into_values().collect::<Vec<_>>(),"reader_sites":readers.into_values().collect::<Vec<_>>(),"inline_sites":literals.into_values().collect::<Vec<_>>(),"inline_sql_occurrences":inline,"tableless_occurrences":tableless,"unmounted_sources":unmounted,"test_only_source_ranges":test_only_source,"diagnostics":local_errors}));
    }
    for report in &mut reports {
        let name = report["crate"].as_str().unwrap();
        let prefix = format!("crates/{name}/sql/");
        let own = assets
            .iter()
            .filter(|a| a.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        let used = own.iter().filter(|a| global_used.contains(*a)).count();
        let mut asset_sql_diagnostics = Vec::new();
        let rows = own.iter().map(|a| {
            let analysis = sql_targets::analyze(&snapshot.blobs[a].text);
            if global_used.contains(a) {
                asset_sql_diagnostics.extend(analysis.diagnostics.iter().map(|error| format!("{a}: {error}")));
            }
            json!({"path":a,"blob":snapshot.blobs[a].oid,"production_read":global_used.contains(a),"classification":if global_used.contains(a) {"production-read"} else if global_test_used.contains(a) {"test-only-read"} else if test_coverage_incomplete {"test-read-unknown"} else {"unread"},"statement":analysis.statement,"diagnostics":analysis.diagnostics,"targets":analysis.targets.iter().map(|t|json!({"relation":t.relation,"role":t.role,"dynamic":t.dynamic})).collect::<Vec<_>>()})
        }).collect::<Vec<_>>();
        asset_sql_diagnostics.sort();
        asset_sql_diagnostics.dedup();
        report["assets"] = json!(rows);
        report["asset_sql_diagnostics"] = json!(asset_sql_diagnostics);
        report["asset_total"] = json!(own.len());
        report["asset_production"] = json!(used);
        let definite = own.iter().filter(|a| global_definite.contains(*a)).count();
        report["asset_production_definite"] = json!(definite);
        report["asset_production_conditional"] = json!(used - definite);
        report["test_only_assets"] = json!(own
            .iter()
            .filter(|a| !global_used.contains(*a) && global_test_used.contains(*a))
            .collect::<Vec<_>>());
        report["test_only_asset_count"] =
            json!(report["test_only_assets"].as_array().unwrap().len());
        report["test_read_unknown_assets"] = json!(own
            .iter()
            .filter(|a| test_coverage_incomplete
                && !global_used.contains(*a)
                && !global_test_used.contains(*a))
            .collect::<Vec<_>>());
        report["test_read_unknown_asset_count"] =
            json!(report["test_read_unknown_assets"].as_array().unwrap().len());
        report["asset_classification_complete"] = json!(!test_coverage_incomplete);
        report["unread_assets"] = json!(own
            .iter()
            .filter(|a| !test_coverage_incomplete
                && !global_used.contains(*a)
                && !global_test_used.contains(*a))
            .collect::<Vec<_>>());
        report["unread_asset_count"] = json!(report["unread_assets"].as_array().unwrap().len());
        report["reader_site_count"] = json!(report["reader_sites"].as_array().unwrap().len());
        report["production_sql"] =
            json!(used + report["inline_sql_occurrences"].as_u64().unwrap() as usize);
        report["production_complete"] = json!(
            report["diagnostics"].as_array().unwrap().is_empty()
                && report["asset_sql_diagnostics"]
                    .as_array()
                    .unwrap()
                    .is_empty()
        );
        report["complete"] =
            json!(report["production_complete"] == true && !test_coverage_incomplete);
    }
    if test_coverage_incomplete {
        diagnostics.push("selected test-source coverage is incomplete; unobserved asset test-read classification is unknown".into());
    }
    diagnostics.sort();
    diagnostics.dedup();
    let complete = diagnostics.is_empty() && reports.iter().all(|r| r["complete"] == true);
    json!({"tool_version":env!("CARGO_PKG_VERSION"),"audited_commit":snapshot.commit,"audited_tree":snapshot.tree,"mode":"test=false source-union; not compiled reachability or SQL validity","count_equation":"production_sql = distinct production-read primary assets + inline table-target sites","complete":complete,"diagnostics":diagnostics,"crates":reports,"input_blobs":snapshot.blobs.iter().map(|(p,b)|json!({"path":p,"mode":b.mode,"oid":b.oid})).collect::<Vec<_>>()})
}
#[cfg(test)]
#[path = "production_controls.rs"]
mod controls;
