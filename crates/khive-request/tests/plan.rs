use std::collections::BTreeMap;

use khive_request::{plan_request, MAX_OPS, MAX_OPS_INPUT_LEN, NESTING_DEPTH_LIMIT};
use serde_json::json;

fn catalog() -> BTreeMap<String, String> {
    [
        ("create", "kg"),
        ("get", "kg"),
        ("memory.remember", "memory"),
    ]
    .into_iter()
    .map(|(verb, pack)| (verb.to_owned(), pack.to_owned()))
    .collect()
}

#[test]
fn chain_accounts_for_stages_and_preserves_nested_references() {
    let plan = plan_request(
        r#"create(content="line\n\"quoted\"", flags=[true, null, 3.5])
        | memory.remember(z=$prev[0].id, a={"z":[$prev, $prev.id, {"nested":$prev.items[2].name}], "a":$prev.id}, literal="\\$prev.id")
        | get(id=$prev.id)"#,
        &catalog(),
    );
    assert_eq!(
        plan,
        json!({
            "parsed": true,
            "mode": "chain",
            "stage_count": 3,
            "stages": [
                {
                    "index": 0, "verb": "create", "pack": "kg", "known": true,
                    "args": {"content": "line\n\"quoted\"", "flags": [true, null, 3.5]},
                    "prev_refs": []
                },
                {
                    "index": 1, "verb": "memory.remember", "pack": "memory", "known": true,
                    "args": {
                        "a": {"z": ["$prev", "$prev.id", {"nested": "$prev.items[2].name"}], "a": "$prev.id"},
                        "literal": "$prev.id",
                        "z": "$prev[0].id"
                    },
                    "prev_refs": ["", "id", "items.[2].name", "id", "[0].id"]
                },
                {
                    "index": 2, "verb": "get", "pack": "kg", "known": true,
                    "args": {"id": "$prev.id"}, "prev_refs": ["id"]
                }
            ],
            "limits": {
                "max_ops": MAX_OPS,
                "max_depth": NESTING_DEPTH_LIMIT,
                "max_input_len": MAX_OPS_INPUT_LEN
            }
        })
    );
}

#[test]
fn quoted_references_retain_parser_paths() {
    let plan = plan_request(
        r#"start() | finish(root="$prev[0].id", nested="$prev.items[2].name", all="$prev")"#,
        &BTreeMap::new(),
    );
    assert_eq!(plan["parsed"], true);
    assert_eq!(
        plan["stages"][1]["prev_refs"],
        json!(["", "items[2].name", "[0].id"])
    );
    assert_eq!(
        plan["stages"][1]["args"],
        json!({"all": "$prev", "nested": "$prev.items[2].name", "root": "$prev[0].id"})
    );
}

#[test]
fn unknown_verb_retains_requested_name_without_parse_error() {
    let plan = plan_request("missing.action(value=4)", &catalog());
    assert_eq!(plan["parsed"], true);
    assert_eq!(plan["mode"], "single");
    assert_eq!(plan["stage_count"], 1);
    assert_eq!(
        plan["stages"],
        json!([{
            "index": 0,
            "verb": "missing.action",
            "pack": null,
            "known": false,
            "args": {"value": 4},
            "prev_refs": []
        }])
    );
    assert!(plan.get("error").is_none());
}

#[test]
fn plan_preserves_parser_modes_for_function_and_json_forms() {
    for (input, mode, count) in [
        ("create()", "single", 1),
        (
            r#"{"tool":"create","args":{"value":{"items":[1,2]}}}"#,
            "single",
            1,
        ),
        ("[create(), get(id=\"x\")]", "parallel", 2),
        (
            r#"[{"tool":"create"},{"tool":"get","args":{"id":"x"}}]"#,
            "parallel",
            2,
        ),
    ] {
        let plan = plan_request(input, &catalog());
        assert_eq!(plan["parsed"], true, "{input}");
        assert_eq!(plan["mode"], mode, "{input}");
        assert_eq!(plan["stage_count"], count, "{input}");
    }
}

#[test]
fn parse_failure_has_exact_error_and_limits_without_partial_stages() {
    let input = "create() | get(id=";
    let error = khive_request::parse_request(input).unwrap_err();
    assert_eq!(
        plan_request(input, &catalog()),
        json!({
            "parsed": false,
            "error": error.to_string(),
            "limits": {
                "max_ops": MAX_OPS,
                "max_depth": NESTING_DEPTH_LIMIT,
                "max_input_len": MAX_OPS_INPUT_LEN
            }
        })
    );
}

#[test]
fn plan_module_dependencies_are_limited_to_parser_types_and_json() {
    assert_planner_dependencies(include_str!("../src/plan.rs"));
    let _: fn(&str, &BTreeMap<String, String>) -> serde_json::Value = plan_request;
}

fn assert_planner_dependencies(source: &str) {
    use syn::visit::Visit;

    struct Dependencies;

    fn check_path(path: &[String]) {
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let allowed = match names.as_slice() {
            ["crate", "parser", "parse_request"] => true,
            ["crate", "types", name] => matches!(
                *name,
                "ArgValue"
                    | "ExecutionMode"
                    | "MAX_OPS"
                    | "MAX_OPS_INPUT_LEN"
                    | "NESTING_DEPTH_LIMIT"
            ),
            ["std", "collections", "BTreeMap"] => true,
            ["serde_json", name] => matches!(*name, "json" | "Map" | "Value"),
            [root, ..] if names.len() > 1 => {
                matches!(
                    *root,
                    "ArgValue" | "ExecutionMode" | "BTreeMap" | "Map" | "Value" | "Vec"
                )
            }
            [_] => true,
            _ => false,
        };
        assert!(
            allowed,
            "unexpected planner dependency: {}",
            names.join("::")
        );
    }

    fn check_use(tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                check_use(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(name) => {
                prefix.push(name.ident.to_string());
                check_path(prefix);
                prefix.pop();
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    check_use(item, prefix);
                }
            }
            _ => panic!("planner dependencies must use explicit, unaliased imports"),
        }
    }

    impl<'ast> Visit<'ast> for Dependencies {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            check_use(&item.tree, &mut Vec::new());
        }

        fn visit_path(&mut self, path: &'ast syn::Path) {
            check_path(
                &path
                    .segments
                    .iter()
                    .map(|part| part.ident.to_string())
                    .collect::<Vec<_>>(),
            );
            syn::visit::visit_path(self, path);
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            self.visit_path(&mac.path);
            for expression in macro_expressions(mac) {
                self.visit_expr(&expression);
            }
        }

        fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {
            panic!("the pure planner must not delegate to another module");
        }

        fn visit_item_extern_crate(&mut self, _: &'ast syn::ItemExternCrate) {
            panic!("the pure planner must not import another crate");
        }
    }

    let module = syn::parse_file(source).unwrap();
    Dependencies.visit_file(&module);
}

fn macro_expressions(mac: &syn::Macro) -> Vec<syn::Expr> {
    use syn::parse::Parser;

    let name = mac.path.segments.last().unwrap().ident.to_string();
    match name.as_str() {
        "json" => {
            let object = |input: syn::parse::ParseStream<'_>| {
                let fields;
                syn::braced!(fields in input);
                let mut values = Vec::new();
                while !fields.is_empty() {
                    let _: syn::LitStr = fields.parse()?;
                    let _: syn::Token![:] = fields.parse()?;
                    values.push(fields.parse::<syn::Expr>()?);
                    if !fields.is_empty() {
                        let _: syn::Token![,] = fields.parse()?;
                    }
                }
                Ok(values)
            };
            object
                .parse2(mac.tokens.clone())
                .expect("planner JSON values must be inspectable Rust expressions")
        }
        "format" => syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(mac.tokens.clone())
            .expect("planner formatting arguments must be inspectable Rust expressions")
            .into_iter()
            .collect(),
        _ => panic!("unexpected planner macro: {name}"),
    }
}

#[test]
fn planner_dependency_guard_rejects_effect_calls_in_macros() {
    let planner = include_str!("../src/plan.rs");
    let mutated = planner.replace(
        "\"parsed\": true,",
        "\"parsed\": std::fs::metadata(\"unused\").is_ok(),",
    );
    assert_ne!(mutated, planner, "the mutation must change the planner");
    assert!(std::panic::catch_unwind(|| assert_planner_dependencies(&mutated)).is_err());
}
