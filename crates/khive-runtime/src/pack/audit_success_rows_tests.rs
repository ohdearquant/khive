use std::collections::{HashMap, HashSet};
use std::process::Command;

use async_trait::async_trait;
use khive_types::Pack;
use serde_json::{json, Value};

use crate::runtime::NamespaceToken;
use crate::RuntimeError;

use super::{
    AuditSuccessRowForcingRule, AuditSuccessRowsMode, HandlerDef, PackRuntime, VerbCategory,
    VerbRegistry, VerbRegistryBuilder, Visibility,
};

const fn handler(name: &'static str, visibility: Visibility, category: VerbCategory) -> HandlerDef {
    HandlerDef {
        name,
        description: "success audit policy fixture",
        visibility,
        category,
        params: &[],
    }
}

const COMM_HANDLERS: &[HandlerDef] = &[
    handler("comm.inbox", Visibility::Verb, VerbCategory::Assertive),
    handler("comm.probe", Visibility::Verb, VerbCategory::Assertive),
    handler(
        "comm.cursor_get",
        Visibility::Subhandler,
        VerbCategory::Assertive,
    ),
    handler(
        "comm.heartbeat",
        Visibility::Subhandler,
        VerbCategory::Declaration,
    ),
    handler(
        "comm.cleanup_expired_quarantine",
        Visibility::Subhandler,
        VerbCategory::Declaration,
    ),
];
const KG_HANDLERS: &[HandlerDef] = &[
    handler("whoami", Visibility::Verb, VerbCategory::Assertive),
    handler("stats", Visibility::Verb, VerbCategory::Assertive),
];
const SHADOW_HANDLERS: &[HandlerDef] = &[
    handler(
        "comm.cursor_get",
        Visibility::Subhandler,
        VerbCategory::Assertive,
    ),
    handler(
        "comm.heartbeat",
        Visibility::Subhandler,
        VerbCategory::Declaration,
    ),
];

struct PolicyPack(&'static str, &'static [HandlerDef]);

impl Pack for PolicyPack {
    const NAME: &'static str = "policy_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for PolicyPack {
    fn name(&self) -> &str {
        self.0
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        self.1
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({"owner": self.0, "outcome": "success"}))
    }
}

fn source_handler_names(source: &str, table: &str) -> Vec<String> {
    let file = syn::parse_file(source).expect("canonical handler source parses");
    let item = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Static(item) if item.ident == table => Some(item),
            _ => None,
        })
        .expect("canonical handler table exists");
    let syn::Expr::Array(array) = item.expr.as_ref() else {
        panic!("handler table is an array")
    };
    array
        .elems
        .iter()
        .map(|expr| {
            let syn::Expr::Struct(definition) = expr else {
                panic!("handler table entry is a struct")
            };
            definition
                .fields
                .iter()
                .find_map(|field| {
                    if !matches!(&field.member, syn::Member::Named(name) if name == "name") {
                        return None;
                    }
                    match &field.expr {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(name),
                            ..
                        }) => Some(name.value()),
                        _ => None,
                    }
                })
                .expect("handler name is a literal")
        })
        .collect()
}

#[test]
fn audit_success_rows_constants_and_registration_boundary() {
    use AuditSuccessRowForcingRule::{PollNotSuccessful, QuarantineChanged};
    let omitted = [
        ("comm", "comm.inbox"),
        ("comm", "comm.probe"),
        ("comm", "comm.cursor_get"),
        ("kg", "whoami"),
    ];
    let sampled = [
        ("comm", "comm.heartbeat", 10, PollNotSuccessful),
        (
            "comm",
            "comm.cleanup_expired_quarantine",
            10,
            QuarantineChanged,
        ),
    ];
    assert_eq!(VerbRegistry::AUDIT_SUCCESS_ROW_OMITTED_VERBS, omitted);
    assert_eq!(VerbRegistry::AUDIT_SUCCESS_ROW_SAMPLED_VERBS, sampled);
    let omitted_names: HashSet<_> = omitted.iter().map(|&(_, verb)| verb).collect();
    let sampled_policies: HashMap<_, _> = sampled
        .iter()
        .map(|&(_, verb, every, forcing)| (verb, (every, forcing)))
        .collect();
    assert!(sampled_policies
        .keys()
        .all(|verb| !omitted_names.contains(verb)));
    for (owner, source, table) in [
        (
            "comm",
            include_str!("../../../khive-pack-comm/src/vocab.rs"),
            "COMM_HANDLERS",
        ),
        (
            "kg",
            include_str!("../../../khive-pack-kg/src/handler_defs.rs"),
            "KG_HANDLERS",
        ),
    ] {
        let names = source_handler_names(source, table);
        for (pack, verb) in omitted
            .iter()
            .copied()
            .chain(sampled.iter().map(|&(p, v, _, _)| (p, v)))
        {
            if pack == owner {
                assert_eq!(
                    names.iter().filter(|name| *name == verb).count(),
                    1,
                    "{owner}/{verb}"
                );
            }
        }
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
    builder.register_trusted(PolicyPack("comm", COMM_HANDLERS));
    builder.register_trusted(PolicyPack("kg", KG_HANDLERS));
    let trusted = builder.build().expect("trusted registry");
    assert_eq!(*trusted.audit_success_row_omitted_verbs, omitted_names);
    assert_eq!(*trusted.audit_success_row_sampled_verbs, sampled_policies);
    assert!(!trusted.audit_success_row_omitted_verbs.contains("stats"));
    assert!(!trusted
        .audit_success_row_sampled_verbs
        .contains_key("stats"));
    assert!(!trusted.admission_degrade_safe_probe("comm.cursor_get"));
    for owner in ["comm", "extension"] {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
        builder.register(PolicyPack(owner, COMM_HANDLERS));
        let untrusted = builder.build().expect("untrusted registry");
        assert!(untrusted.audit_success_row_omitted_verbs.is_empty());
        assert!(untrusted.audit_success_row_sampled_verbs.is_empty());
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
    builder.register_trusted(PolicyPack("extension", COMM_HANDLERS));
    let wrong_owner = builder.build().expect("trusted noncanonical owner");
    assert!(wrong_owner.audit_success_row_omitted_verbs.is_empty());
    assert!(wrong_owner.audit_success_row_sampled_verbs.is_empty());
}

#[tokio::test]
async fn audit_success_rows_effective_first_owner_controls_membership() {
    for shadow_trusted in [false, true] {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
        let shadow = PolicyPack("extension", SHADOW_HANDLERS);
        if shadow_trusted {
            builder.register_trusted(shadow);
        } else {
            builder.register(shadow);
        }
        builder.register_trusted(PolicyPack("comm", COMM_HANDLERS));
        let registry = builder.build().expect("earlier shadow registry");
        assert!(!registry
            .audit_success_row_omitted_verbs
            .contains("comm.cursor_get"));
        assert!(!registry
            .audit_success_row_sampled_verbs
            .contains_key("comm.heartbeat"));
        for verb in ["comm.cursor_get", "comm.heartbeat"] {
            assert_eq!(
                registry
                    .dispatch(verb, json!({}))
                    .await
                    .expect("shadow dispatch")["owner"],
                "extension"
            );
        }
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
    builder.register_trusted(PolicyPack("comm", COMM_HANDLERS));
    builder.register(PolicyPack("extension", SHADOW_HANDLERS));
    let registry = builder.build().expect("canonical owner first");
    assert!(registry
        .audit_success_row_omitted_verbs
        .contains("comm.cursor_get"));
    assert!(registry
        .audit_success_row_sampled_verbs
        .contains_key("comm.heartbeat"));
    assert_eq!(
        registry
            .dispatch("comm.heartbeat", json!({}))
            .await
            .expect("canonical dispatch")["owner"],
        "comm"
    );
}

#[test]
fn audit_success_rows_mode_is_registry_scoped() {
    const CASE: &str = "KHIVE_TEST_AUDIT_SUCCESS_ROWS_CASE";
    const ENV: &str = "KHIVE_AUDIT_SUCCESS_ROWS";
    if let Ok(case) = std::env::var(CASE) {
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(std::io::stderr)
            .try_init()
            .expect("isolated trace capture");
        let mut builder = VerbRegistryBuilder::new();
        let expected = match case.as_str() {
            "unset" => AuditSuccessRowsMode::Reduced,
            "all" => {
                std::env::set_var(ENV, "all");
                AuditSuccessRowsMode::All
            }
            "reduced" => {
                std::env::set_var(ENV, "reduced");
                AuditSuccessRowsMode::Reduced
            }
            "unknown" => {
                std::env::set_var(ENV, "invalid-policy");
                AuditSuccessRowsMode::All
            }
            "override_all" => {
                std::env::set_var(ENV, "reduced");
                builder.with_audit_success_rows_mode(AuditSuccessRowsMode::All);
                AuditSuccessRowsMode::All
            }
            "override_reduced" => {
                std::env::set_var(ENV, "all");
                builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
                AuditSuccessRowsMode::Reduced
            }
            "override_unknown" => {
                std::env::set_var(ENV, "invalid-policy");
                builder.with_audit_success_rows_mode(AuditSuccessRowsMode::Reduced);
                AuditSuccessRowsMode::Reduced
            }
            "stable" => {
                std::env::set_var(ENV, "reduced");
                AuditSuccessRowsMode::Reduced
            }
            _ => panic!("unknown test case"),
        };
        let registry = builder.build().expect("mode registry");
        assert!(registry.event_store().is_none());
        assert!(registry.audit_batch_metrics().is_none());
        assert_eq!(registry.audit_success_rows_mode(), expected);
        assert_eq!(
            registry.audit_success_rows_mode().as_str(),
            if expected == AuditSuccessRowsMode::Reduced {
                "reduced"
            } else {
                "all"
            }
        );
        if case == "stable" {
            std::env::set_var(ENV, "all");
            assert_eq!(
                registry.audit_success_rows_mode(),
                AuditSuccessRowsMode::Reduced
            );
            assert_eq!(
                registry.clone().audit_success_rows_mode(),
                AuditSuccessRowsMode::Reduced
            );
            assert_eq!(
                VerbRegistryBuilder::new()
                    .build()
                    .expect("new registry")
                    .audit_success_rows_mode(),
                AuditSuccessRowsMode::All
            );
            std::env::set_var(ENV, "reduced");
        }
        return;
    }
    for case in [
        "unset",
        "all",
        "reduced",
        "unknown",
        "override_all",
        "override_reduced",
        "override_unknown",
        "stable",
    ] {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "pack::audit_success_rows_tests::audit_success_rows_mode_is_registry_scoped",
                "--nocapture",
            ])
            .env_remove(ENV)
            .env(CASE, case)
            .output()
            .expect("isolated mode test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "mode child must execute exactly one test: {stdout}"
        );
        assert_eq!(
            stderr
                .matches("unrecognized KHIVE_AUDIT_SUCCESS_ROWS; using all")
                .count(),
            usize::from(case == "unknown"),
            "{case}: {stderr}"
        );
        if case == "unknown" {
            assert!(
                stderr.contains("invalid-policy"),
                "warning names value: {stderr}"
            );
        }
    }
}
