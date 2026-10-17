/// help=true on `create` returns a schema envelope with the correct verb name,
/// pack name, description, and at least the required `kind` parameter.
#[tokio::test]
async fn test_help_true_returns_schema_for_kg_create() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    let result = reg
        .dispatch("create", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for a known verb");

    // Shape checks.
    assert_eq!(result["verb"], "create", "envelope must name the verb");
    assert_eq!(
        result["pack"], "helptest",
        "envelope must name the owning pack"
    );
    assert!(
        result["description"].as_str().is_some(),
        "description must be a string"
    );

    // Params array must be present and non-empty.
    let params = result["params"]
        .as_array()
        .expect("params must be a JSON array");
    assert!(!params.is_empty(), "params array must not be empty");

    // The required `kind` param must appear.
    let kind_param = params.iter().find(|p| p["name"] == "kind");
    assert!(
        kind_param.is_some(),
        "params array must include the 'kind' parameter"
    );
    let kind_param = kind_param.unwrap();
    assert_eq!(
        kind_param["required"],
        serde_json::json!(true),
        "'kind' must be required"
    );
    assert_eq!(kind_param["type"], "string", "'kind' type must be 'string'");

    let identifier_help = result["identifier_resolution"]
        .as_object()
        .expect("help=true must include the shared identifier contract");
    assert!(identifier_help["full_uuid"]
        .as_str()
        .is_some_and(|text| text.contains("globally unique")));
    assert!(identifier_help["short_prefix"]
        .as_str()
        .is_some_and(|text| text.contains("lookup scope belongs to the consuming parameter")));
    assert!(identifier_help["parameter_rule"]
        .as_str()
        .is_some_and(|text| text.contains("submitted again")));
}

#[test]
fn event_target_resolution_metadata_has_a_conditional_contract() {
    let mode = IdResolutionMode::EdgeOrEventTarget;
    let text = resolution_mode_contract(mode).unwrap();
    assert!(text.contains("kind=event accepts only a full subject UUID"));
    assert!(text.contains("prefixes and names are rejected without graph resolution"));
    assert!(text.contains("For kind=edge"));
    assert!(text.contains("prefix or entity name resolves in the primary namespace"));
    assert_eq!(resolution_mode_key(mode), "edge_or_event_target");
    assert_eq!(
        identifier_resolution_help()["resolution_modes"]["edge_or_event_target"],
        text
    );
}

/// `describe_verb` appends `resolution_mode_contract(p.resolution_mode)`
/// to every uuid-typed parameter's description, so the full-UUID-vs-
/// short-prefix rule is stated once per mode (in
/// `resolution_mode_contract`) and inherited by every verb declaring
/// that mode, rather than requiring each `HandlerDef` to paste its own
/// explanation — or, worse, one blanket explanation getting appended to
/// parameters whose actual resolver behaves differently (see
/// `IdResolutionMode`'s doc comment: this is exactly the bug the mode
/// field replaced — a single shared string asserted namespace-agnostic
/// full-UUID acceptance on every uuid param, which was false for
/// primary-scoped and namespace-filtered resolvers).
/// This test covers `IdResolutionMode::UnscopedById`; the full mode
/// enumeration is exercised against real pack definitions by
/// `kkernel::pack_introspect::tests::
/// every_uuid_param_across_every_registered_pack_declares_a_resolution_mode`
/// and its `describe_verb_renders_mode_specific_contract_for_real_handlers`
/// companion (khive-runtime cannot depend on the pack crates itself
/// without a circular dependency).
///
/// The namespace assertion below is deliberately specific. It first
/// asserted only `description.contains("namespace")`, which passes
/// identically whether the contract says prefix resolution *is* or *is
/// not* namespace-scoped — so it passed while the contract stated the
/// opposite of what the by-ID verbs do. A test that cannot separate the
/// two readings protects neither.
#[tokio::test]
async fn test_help_true_uuid_param_carries_shared_id_contract() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    let result = reg
        .dispatch("get", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for a known verb");

    let params = result["params"]
        .as_array()
        .expect("params must be a JSON array");
    let id_param = params
        .iter()
        .find(|p| p["name"] == "id")
        .expect("params array must include the 'id' parameter");
    let description = id_param["description"]
        .as_str()
        .expect("description must be a string");

    // The verb-specific text must still be present...
    assert!(
        description.contains("UUID of the record to fetch"),
        "shared contract must be appended, not replace, the verb-specific text; got: {description}"
    );
    // ...and the shared contract must state the ACTUAL by-ID rule, in a
    // form that separates it from its negation. Must-match and
    // must-not-match together: either arm alone still admits a contract
    // that merely mentions namespaces without committing to a rule.
    assert!(
        description.contains("no namespace filter"),
        "id contract must state that by-ID prefix resolution applies NO namespace filter \
             (ADR-007 Rev 6, `resolve_prefix_unfiltered`); got: {description}"
    );
    assert!(
        !description.contains("namespace-scoped resolution"),
        "id contract must not claim prefix resolution is namespace-scoped — get/update/\
             delete/merge resolve prefixes unfiltered; got: {description}"
    );
    assert!(
        description.to_ascii_lowercase().contains("prefix"),
        "id contract must describe short-prefix semantics; got: {description}"
    );

    // `link`'s source_id/target_id are `param_type: "uuid"` too (they
    // resolve through the same unfiltered path as get/update/delete/
    // merge — see `crates/khive-pack-kg/src/handlers/link.rs`), so the
    // contract's enumerated verb list must name `link` explicitly, not
    // just the four record-level by-ID verbs.
    let link_result = reg
        .dispatch("link", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for link");
    let link_params = link_result["params"]
        .as_array()
        .expect("link params must be a JSON array");
    let source_id_param = link_params
        .iter()
        .find(|p| p["name"] == "source_id")
        .expect("link params must include 'source_id'");
    let source_id_description = source_id_param["description"]
        .as_str()
        .expect("description must be a string");
    assert!(
        source_id_description.contains("get/update/delete/merge/link"),
        "id contract's by-ID verb enumeration must include 'link' alongside get/update/\
             delete/merge, since link's source_id/target_id resolve through the same \
             unfiltered path; got: {source_id_description}"
    );
}
