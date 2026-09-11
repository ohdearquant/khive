fn format_expressions(mac: &syn::Macro) -> Vec<syn::Expr> {
    use syn::parse::Parser;

    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
        .parse2(mac.tokens.clone())
        .expect("plan error formatting arguments must be inspectable Rust expressions")
        .into_iter()
        .collect()
}

fn adapter_method(source: &str, owner: &str, name: &str) -> syn::ImplItemFn {
    let file = syn::parse_file(source).unwrap();
    let mut methods = Vec::new();
    for item in file.items {
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
        let syn::Type::Path(ty) = *implementation.self_ty else {
            continue;
        };
        if !ty.path.is_ident(owner) {
            continue;
        }
        for item in implementation.items {
            if let syn::ImplItem::Fn(method) = item {
                if method.sig.ident == name {
                    methods.push(method);
                }
            }
        }
    }
    assert_eq!(methods.len(), 1, "expected one {owner}::{name} adapter");
    methods.pop().unwrap()
}

fn assert_adapter_dependencies(method: &syn::ImplItemFn) {
    use std::collections::BTreeSet;
    use syn::visit::Visit;

    struct Calls<'a> {
        allowed_calls: &'a [&'a str],
        allowed_methods: &'a [&'a str],
        allowed_fields: &'a [&'a str],
        calls: BTreeSet<String>,
        methods: BTreeSet<String>,
        order: Vec<String>,
    }

    fn path_name(path: &syn::Path) -> String {
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::")
    }

    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            let syn::Expr::Path(function) = call.func.as_ref() else {
                panic!("plan adapters must not invoke dynamic callbacks");
            };
            let name = path_name(&function.path);
            assert!(
                self.allowed_calls.contains(&name.as_str()),
                "unexpected adapter call: {name}"
            );
            self.order.push(name.clone());
            self.calls.insert(name);
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            let name = call.method.to_string();
            assert!(
                self.allowed_methods.contains(&name.as_str()),
                "unexpected adapter method: {name}"
            );
            if matches!(name.as_str(), "map" | "find") {
                assert!(
                    matches!(call.args.first(), Some(syn::Expr::Closure(_))),
                    "plan adapter callbacks must be inline and inspectable"
                );
            }
            self.order.push(name.clone());
            self.methods.insert(name);
            syn::visit::visit_expr_method_call(self, call);
        }

        fn visit_expr_field(&mut self, field: &'ast syn::ExprField) {
            let syn::Expr::Path(base) = field.base.as_ref() else {
                panic!("plan adapters must not traverse unrelated state");
            };
            let syn::Member::Named(member) = &field.member else {
                panic!("unexpected tuple field in plan adapter");
            };
            let name = format!("{}.{}", path_name(&base.path), member);
            assert!(
                self.allowed_fields.contains(&name.as_str()),
                "unexpected adapter field: {name}"
            );
            syn::visit::visit_expr_field(self, field);
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            assert!(
                mac.path.is_ident("format"),
                "plan adapter macros must only format error text"
            );
            for expression in format_expressions(mac) {
                self.visit_expr(&expression);
            }
        }

        fn visit_expr_struct(&mut self, _: &'ast syn::ExprStruct) {
            panic!("plan adapters must not construct runtime objects");
        }
    }

    let (calls, methods, fields): (&[&str], &[&str], &[&str]) =
        match method.sig.ident.to_string().as_str() {
            "plan_ops" => (
                &["khive_request::plan_request"],
                // The mounted tool catalog is a second read of the registry
                // (a pinned snapshot, no subprocess call); merging it into the
                // plan catalog adds iterator and JSON accessor methods only.
                &[
                    "all_verbs_with_names",
                    "mounted_verb_snapshot",
                    "into_iter",
                    "map",
                    "chain",
                    "as_str",
                    "unwrap_or_default",
                    "to_owned",
                    "collect",
                    "to_string",
                ],
                &["self.registry", "handler.name"],
            ),
            "plan_response" => (
                &["Some", "Ok"],
                &["validate_plan_envelope", "plan_ops"],
                &["p.plan", "p.ops"],
            ),
            "validate_plan_envelope" => (
                &["Err", "Ok", "rmcp::ErrorData::invalid_params"],
                &["is_some", "iter", "zip", "find"],
                &[
                    "self.presentation",
                    "self.presentation_per_op",
                    "self.format",
                    "self.format_per_op",
                    "self.save_to",
                    "self.request_id",
                ],
            ),
            name => panic!("unexpected plan adapter: {name}"),
        };
    let mut guard = Calls {
        allowed_calls: calls,
        allowed_methods: methods,
        allowed_fields: fields,
        calls: BTreeSet::new(),
        methods: BTreeSet::new(),
        order: Vec::new(),
    };
    guard.visit_block(&method.block);
    assert_eq!(
        guard.calls,
        calls.iter().map(|name| (*name).to_owned()).collect()
    );
    assert_eq!(
        guard.methods,
        methods.iter().map(|name| (*name).to_owned()).collect()
    );
    let before = match method.sig.ident.to_string().as_str() {
        "plan_ops" => Some(("all_verbs_with_names", "khive_request::plan_request")),
        "plan_response" => Some(("validate_plan_envelope", "plan_ops")),
        _ => None,
    };
    if let Some((first, second)) = before {
        assert!(
            guard.order.iter().position(|name| name == first).unwrap()
                < guard.order.iter().position(|name| name == second).unwrap()
        );
    }
}

#[test]
fn mcp_plan_adapters_only_validate_presence_read_catalog_and_plan() {
    let server = include_str!("../src/server.rs");
    for name in ["plan_ops", "plan_response"] {
        assert_adapter_dependencies(&adapter_method(server, "KhiveMcpServer", name));
    }
    let request = include_str!("../src/tools/request.rs");
    assert_adapter_dependencies(&adapter_method(
        request,
        "RequestParams",
        "validate_plan_envelope",
    ));
}

#[test]
fn mcp_plan_adapter_guards_reject_effect_calls() {
    for (source, owner, name) in [
        (
            include_str!("../src/server.rs"),
            "KhiveMcpServer",
            "plan_ops",
        ),
        (
            include_str!("../src/server.rs"),
            "KhiveMcpServer",
            "plan_response",
        ),
        (
            include_str!("../src/tools/request.rs"),
            "RequestParams",
            "validate_plan_envelope",
        ),
    ] {
        let mut method = adapter_method(source, owner, name);
        method
            .block
            .stmts
            .insert(0, syn::parse_quote!(crate::gate::admit();));
        assert!(
            std::panic::catch_unwind(|| assert_adapter_dependencies(&method)).is_err(),
            "{owner}::{name} accepted the mutation"
        );
    }
}
