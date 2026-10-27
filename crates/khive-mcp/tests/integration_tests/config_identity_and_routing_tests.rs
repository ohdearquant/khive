// =============================================================================
// ADR-096 Fork 1 completion: identity-derived visibility is carried per request,
// not in the daemon engine-coherence key.
// =============================================================================

/// Two RuntimeConfigs that are identical except for their `visible_namespaces`
/// must produce the same `compute_config_id` fingerprint.
///
/// `visible_namespaces` is a per-request identity field in the daemon frame.
/// Keeping it out of the engine-coherence key lets seats with different folded
/// actor visibility share one warm daemon while still dispatching under their
/// own frame identity.
#[test]
fn compute_config_id_is_identical_when_only_visible_namespaces_differ() {
    use khive_mcp::server::compute_config_id;

    let mut base = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    base.default_namespace = Namespace::parse("vis-a").unwrap();
    base.visible_namespaces = vec![];

    let with_visible = RuntimeConfig {
        visible_namespaces: vec![Namespace::parse("vis-b").unwrap()],
        ..base.clone()
    };
    let reordered_visible = RuntimeConfig {
        visible_namespaces: vec![
            Namespace::parse("vis-c").unwrap(),
            Namespace::parse("vis-b").unwrap(),
        ],
        ..base.clone()
    };

    assert_eq!(
        compute_config_id(&base, None),
        compute_config_id(&with_visible, None),
        "compute_config_id must ignore visible_namespaces; visibility travels \
         per request in the daemon frame"
    );
    assert_eq!(
        compute_config_id(&with_visible, None),
        compute_config_id(&reordered_visible, None),
        "visible namespace order must also be inert because visible_namespaces \
         are excluded from the fingerprint"
    );
    assert!(
        !compute_config_id(&with_visible, None).contains("vis-b"),
        "visible namespace 'vis-b' must not appear in config_id string"
    );
}

// =============================================================================
// Fix 1: compute_config_id must include allowed_outbound_namespaces so a
// daemon started with a permissive outbound allowlist cannot be reused for a
// client whose config has an empty allowlist.
// =============================================================================

/// Two RuntimeConfigs that are identical except for their
/// `allowed_outbound_namespaces` must produce different `compute_config_id`
/// fingerprints.
///
/// Without this, a daemon started with `allowed_outbound_namespaces =
/// ["lambda:khive"]` could be reused for a client whose local config has an
/// empty allowlist — granting cross-namespace writes that the client should
/// fail closed on.
#[test]
fn compute_config_id_differs_when_allowed_outbound_namespaces_differ() {
    use khive_mcp::server::compute_config_id;

    let mut base = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    base.default_namespace = Namespace::parse("out-a").unwrap();
    base.allowed_outbound_namespaces = vec![];
    let with_outbound = RuntimeConfig {
        allowed_outbound_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..base.clone()
    };

    let id_empty = compute_config_id(&base, None);
    let id_with_outbound = compute_config_id(&with_outbound, None);

    assert_ne!(
        id_empty, id_with_outbound,
        "compute_config_id must differ when allowed_outbound_namespaces differs; \
         same id would allow wrong-allowlist daemon reuse"
    );
    assert!(
        id_with_outbound.contains("lambda:khive"),
        "allowed outbound namespace 'lambda:khive' must appear in config_id string; got: {id_with_outbound}"
    );
}

/// Order of entries in allowed_outbound_namespaces must not change the
/// fingerprint (the fingerprint sorts + deduplicates before hashing).
#[test]
fn compute_config_id_is_stable_under_allowed_outbound_namespace_reorder() {
    use khive_mcp::server::compute_config_id;

    let mut cfg_ab = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    cfg_ab.default_namespace = Namespace::parse("out-a").unwrap();
    cfg_ab.allowed_outbound_namespaces = vec![
        Namespace::parse("lambda:khive").unwrap(),
        Namespace::parse("lambda:alice").unwrap(),
    ];
    let cfg_ba = RuntimeConfig {
        allowed_outbound_namespaces: vec![
            Namespace::parse("lambda:alice").unwrap(),
            Namespace::parse("lambda:khive").unwrap(),
        ],
        ..cfg_ab.clone()
    };
    let cfg_dup = RuntimeConfig {
        allowed_outbound_namespaces: vec![
            Namespace::parse("lambda:khive").unwrap(),
            Namespace::parse("lambda:alice").unwrap(),
            Namespace::parse("lambda:khive").unwrap(), // duplicate
        ],
        ..cfg_ab.clone()
    };

    assert_eq!(
        compute_config_id(&cfg_ab, None),
        compute_config_id(&cfg_ba, None),
        "compute_config_id must be stable under reordering of allowed_outbound_namespaces"
    );
    assert_eq!(
        compute_config_id(&cfg_ab, None),
        compute_config_id(&cfg_dup, None),
        "compute_config_id must be stable under duplication of allowed_outbound_namespaces"
    );
}

// =============================================================================
// ADR-108: the construction-baked git-write policy is part of daemon identity.
// =============================================================================

#[test]
fn compute_config_id_fingerprints_git_write_policy_deterministically_and_in_entry_order() {
    use khive_mcp::server::compute_config_id;

    let base = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    let policy = GitWriteSectionConfig {
        allowed: vec![
            GitWriteEntryConfig {
                repo: "/srv/repos/alpha".to_string(),
                branches: vec!["feat/*".to_string(), "fix/*".to_string()],
            },
            GitWriteEntryConfig {
                repo: "/srv/repos/beta".to_string(),
                branches: vec!["release/*".to_string()],
            },
        ],
        ..Default::default()
    };
    let configured = RuntimeConfig {
        git_write: policy.clone(),
        ..base.clone()
    };
    let identical = RuntimeConfig {
        git_write: policy.clone(),
        ..base.clone()
    };
    let changed = RuntimeConfig {
        git_write: GitWriteSectionConfig {
            allowed: vec![GitWriteEntryConfig {
                repo: "/srv/repos/alpha".to_string(),
                branches: vec!["fix/*".to_string()],
            }],
            ..Default::default()
        },
        ..base.clone()
    };
    let reordered = RuntimeConfig {
        git_write: GitWriteSectionConfig {
            allowed: policy.allowed.into_iter().rev().collect(),
            ..Default::default()
        },
        ..base
    };

    let configured_id = compute_config_id(&configured, None);
    assert_eq!(
        configured_id,
        compute_config_id(&identical, None),
        "identical ordered git-write policies must fingerprint deterministically"
    );
    assert_ne!(
        configured_id,
        compute_config_id(&changed, None),
        "a changed git-write allowlist must invalidate the warm daemon"
    );
    assert_ne!(
        configured_id,
        compute_config_id(&reordered, None),
        "git-write allowlist entry order is semantic because policy evaluation uses the first matching repo"
    );
}

#[test]
fn compute_config_id_normalizes_absent_and_present_but_empty_git_write_to_same_fail_closed_policy()
{
    use khive_mcp::server::compute_config_id;

    let dir = tempfile::tempdir().expect("tempdir");
    let absent_path = dir.path().join("absent.toml");
    let empty_path = dir.path().join("empty.toml");
    std::fs::write(&absent_path, "").expect("write config without git_write section");
    std::fs::write(&empty_path, "[git_write]\n")
        .expect("write config with an empty git_write section");
    let absent_config = KhiveConfig::load(Some(&absent_path))
        .expect("load absent policy config")
        .expect("config exists");
    let empty_config = KhiveConfig::load(Some(&empty_path))
        .expect("load empty policy config")
        .expect("config exists");
    let base = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    let absent = runtime_config_from_khive_config(&absent_config, base.clone());
    let present_but_empty = runtime_config_from_khive_config(&empty_config, base);

    assert_eq!(
        compute_config_id(&absent, None),
        compute_config_id(&present_but_empty, None),
        "both forms resolve to the same empty RuntimeConfig policy and deny every git write"
    );
}

// =============================================================================
// ADR-007 Rev 2: dispatch honors the resolved namespace — an explicit `namespace=`
// request param when supplied, else `default_namespace` (`local` for OSS). Actor
// identity never silently routes storage (Rule 0, enforced at the config layer:
// see runtime.rs `..._actor_id_does_not_override_default_namespace`).
// =============================================================================

/// Proves the dispatch-side namespace contract at the real MCP server boundary.
///
/// All ops go through `dispatch_request_local`, so a regression at the
/// `VerbRegistry::dispatch` mint site (`pack.rs`) is caught here:
///
///   (a) with `default_namespace = local`, a plain `create` lands in `"local"`.
///
///   (b) an explicit `create(namespace="lambda:alice")` lands in `"lambda:alice"` —
///       the caller deliberately targeting a namespace (Rule 1).  This is exactly
///       what #159's unconditional `Namespace::local()` hard-pin wrongly collapsed
///       to `"local"`.
///
///   (c) a default `list` (local) sees the local entity but NOT the lambda:alice one,
///       and `list(namespace="lambda:alice")` sees the lambda:alice entity but NOT the
///       local one — multi-record reads filter by the supplied namespace.
///
/// Regression sensitivity: if dispatch reverts to pinning `Namespace::local()`,
/// assertion (b) fails (the entity namespace would be `"local"`) and the scoped
/// list in (c) would be empty.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dispatch_honors_explicit_namespace_else_local_adr007() {
    use khive_mcp::tools::request::RequestParams;
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    disable_daemon();

    // OSS default: default_namespace = local; actor identity is attribution only.
    let cfg = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    let rt = KhiveRuntime::new(cfg).expect("in-memory runtime");
    let server =
        KhiveMcpServer::with_packs(rt.clone(), &["kg".to_string()]).expect("server builds");

    // Dispatch one `create`/`list` op and return the parsed `results[0]` body.
    async fn dispatch_op(server: &KhiveMcpServer, ops: &str) -> Value {
        let out = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: ops.to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("dispatch must not error at the MCP level");
        let body: Value = serde_json::from_str(&out).expect("response must be valid JSON");
        body["results"][0].clone()
    }

    fn list_ids(result: &Value) -> Vec<String> {
        result["result"]["items"]
            .as_array()
            .unwrap_or_else(|| panic!("list result must contain an items array; got: {result}"))
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    }

    // ── (a) DEFAULT CREATE: lands in "local" ─────────────────────────────────
    let default_res = dispatch_op(&server, r#"create(kind="concept", name="DefaultProbe")"#).await;
    assert_eq!(
        default_res["ok"],
        json!(true),
        "default create must succeed; got: {default_res}"
    );
    assert_eq!(
        default_res["result"]["namespace"].as_str().unwrap_or(""),
        "local",
        "a create with no explicit namespace must land in 'local'; got: {default_res}"
    );
    let default_id = default_res["result"]["id"]
        .as_str()
        .expect("create result must carry 'id'")
        .to_string();

    // ── (b) EXPLICIT CREATE: namespace="lambda:alice" is honored ───────────────
    let named_res = dispatch_op(
        &server,
        r#"create(kind="concept", name="NamedProbe", namespace="lambda:alice")"#,
    )
    .await;
    assert_eq!(
        named_res["ok"],
        json!(true),
        "explicit-namespace create must succeed; got: {named_res}"
    );
    assert_eq!(
        named_res["result"]["namespace"].as_str().unwrap_or(""),
        "lambda:alice",
        "create(namespace=\"lambda:alice\") must land in 'lambda:alice', not be collapsed to \
         'local' (ADR-007 Rev 2 Rule 1 — explicit namespace is honored); got: {named_res}"
    );
    let named_id = named_res["result"]["id"]
        .as_str()
        .expect("create result must carry 'id'")
        .to_string();

    // ── (c) LIST scoping: default(local) vs explicit(lambda:alice) ─────────────
    let local_ids = list_ids(&dispatch_op(&server, r#"list(kind="entity")"#).await);
    assert!(
        local_ids.contains(&default_id),
        "default list must see the local entity; got: {local_ids:?}"
    );
    assert!(
        !local_ids.contains(&named_id),
        "default(local) list must NOT see the lambda:alice entity; got: {local_ids:?}"
    );

    let leo_ids =
        list_ids(&dispatch_op(&server, r#"list(kind="entity", namespace="lambda:alice")"#).await);
    assert!(
        leo_ids.contains(&named_id),
        "list(namespace=lambda:alice) must see the lambda:alice entity; got: {leo_ids:?}"
    );
    assert!(
        !leo_ids.contains(&default_id),
        "list(namespace=lambda:alice) must NOT see the local entity; got: {leo_ids:?}"
    );
}
